//! Running a package script chain with its tests captured.
//!
//! A target that calls a package script made of several commands
//! (`vitest run a && tsc --noEmit && vitest run b`) is run one step at a
//! time, the steps `--related` expands the script into. A test runner step
//! has its own report; the other steps run as written. A failing test is
//! retried by rerunning only its step's runner, narrowed to that test, and
//! the chain goes on after the step passes, as `&&` would have. A step
//! whose only failures are quarantined passes the same way. The results of
//! all steps are one report.

use super::*;
use crate::plugins::test_reports::{ChainPlan, ChainStep};
use crate::test_results::quarantine::Quarantine;

/// What a chain run needs to know about its target.
pub(super) struct ChainJob<'a> {
    pub address: &'a str,
    pub plugin: &'a dyn LanguagePlugin,
    pub plugin_name: &'a str,
    pub project_dir: &'a Path,
    pub working_dir: &'a Path,
    pub target_name: &'a str,
    pub setup: Option<&'a str>,
    pub max_retries: u32,
    pub quarantine: Option<&'a Quarantine>,
    pub null_stdin: bool,
    pub log_cap: usize,
    /// The admission permit every process of the chain is launched through.
    pub permit: Option<&'a Permit>,
}

/// Run every step in order until one fails, retrying and quarantining per
/// step. The result carries one report for the steps that ran. Its
/// confidence is complete only when every step ran and every runner step
/// wrote a complete report: a runner that crashed before writing one leaves
/// the target uncaptured for this run.
pub(super) fn run(job: &ChainJob, chain: &ChainPlan) -> ExecutionResult {
    let mut combined = ExecutionResult {
        address: job.address.to_string(),
        success: true,
        skipped: false,
        cached: false,
        output: String::new(),
        log: Vec::new(),
        duration_ms: 0,
        exit_code: None,
        tests: None,
        input_hash: None,
        interrupted: false,
        blocked: false,
        run_wall_ms: None,
    };
    let mut cases = Vec::new();
    let mut quarantined = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut complete = true;
    let mut ran = 0;
    for step in &chain.steps {
        if shutdown_requested() {
            break;
        }
        ran += 1;
        let (mut result, report) = run_step(job, chain, step);
        retry::append_log(
            &mut combined,
            &format!("$ {}\n", chain.launch.apply(&step.leaf)),
            &result.log,
        );
        combined.duration_ms += result.duration_ms;
        combined.exit_code = result.exit_code;
        if let Some((plan, _)) = &step.plan {
            match report {
                Some(report) => {
                    complete &= report.confidence == Confidence::Complete;
                    cases.extend(report.cases);
                    quarantined.extend(report.quarantined);
                    let name = plan.format.capture_name().to_string();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                None => complete = false,
            }
        }
        if !std::mem::take(&mut result.success) {
            combined.success = false;
            break;
        }
    }
    complete &= ran == chain.steps.len();
    combined.tests = Some(TestReport {
        project: None,
        capture: names.join("+"),
        confidence: if complete {
            Confidence::Complete
        } else {
            Confidence::Partial
        },
        cases,
        quarantined,
    });
    combined
}

/// Run one step. A runner step reads its report, is retried when tests
/// failed in it, and is let through when only quarantined tests still fail.
fn run_step(
    job: &ChainJob,
    chain: &ChainPlan,
    step: &ChainStep,
) -> (ExecutionResult, Option<TestReport>) {
    let shown = chain.launch.apply(&step.leaf);
    let mut invocation = match &step.plan {
        Some((plan, _)) => Invocation::planned(&step.leaf, plan),
        None => Invocation::plain(&step.leaf),
    };
    invocation.command = chain.launch.apply(&invocation.command);
    invocation.shown = shown;
    let started = SystemTime::now();
    let (mut result, raw) = invocation.run(
        job.address,
        job.working_dir,
        job.null_stdin,
        job.log_cap,
        job.permit,
    );
    let Some(plan) = &step.plan else {
        return (result, None);
    };
    result.tests = read_test_report(job.address, &[Some(plan.clone())], &[raw], started);
    if !result.success && job.max_retries > 0 && !shutdown_requested() {
        {
            let plugin = job.plugin;
            let wrappers = Wrappers::default();
            retry::rerun_failures(
                &retry::RetryJob {
                    address: job.address,
                    plugin,
                    project_dir: job.project_dir,
                    working_dir: job.working_dir,
                    target_name: job.target_name,
                    command: &step.leaf,
                    setup: job.setup,
                    wrappers: &wrappers,
                    launch: Some(&chain.launch),
                    declared: None,
                    max_retries: job.max_retries,
                    null_stdin: job.null_stdin,
                    log_cap: job.log_cap,
                    permit: job.permit,
                },
                &mut result,
            );
        }
    }
    if let (Some(quarantine), Some(report), false) =
        (job.quarantine, result.tests.as_ref(), result.success)
    {
        match quarantine.judge(
            job.address,
            job.plugin_name,
            &step.leaf,
            result.exit_code,
            report,
        ) {
            Verdict::Pass(tests) => {
                let mut note = format!(
                    "[aster] Quarantine: every failing test of this step is quarantined, so {} goes on\n",
                    job.address
                );
                for test in &tests {
                    note.push_str(&format!("[aster]   {} ({})\n", test.name, test.source));
                }
                retry::append_log(&mut result, &note, &[]);
                if let Some(report) = result.tests.as_mut() {
                    report.quarantined = tests;
                }
                result.success = true;
            }
            Verdict::Fail(why) => {
                retry::append_log(&mut result, &format!("[aster] Quarantine: {why}\n"), &[]);
            }
            Verdict::Untouched => {}
        }
    }
    let report = result.tests.take();
    (result, report)
}
