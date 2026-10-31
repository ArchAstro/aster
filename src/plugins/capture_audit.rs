//! What a run can do with a test target's results: capture them, let
//! quarantine excuse a failing test, and retry it.
//!
//! The executor plans a target with [`super::test_reports`] and
//! [`super::test_retry`]; this module asks the same functions what would
//! happen, so `aster check` reports exactly the targets a run would fail to
//! capture, quarantine or retry.

use std::path::PathBuf;

use super::related_tests::Wrappers;
use super::test_reports::{self, Chain};
use super::test_retry;
use super::LanguagePlugin;
use crate::config::ReportConfig;
use crate::test_results::quarantine::tests_failed_codes;
use crate::test_results::{
    Confidence, ReportContext, ReportFiles, ReportFormat, ReportPlan, TestCase, TestOutcome,
};

/// One test runner command whose results are read.
struct Step {
    /// The command as it is retried and judged: the target's command, or one
    /// step of a package script chain.
    command: String,
    plan: ReportPlan,
}

/// What the executor does with a target's results.
pub(crate) enum Audit {
    /// No per-test results are read. For a chain, the step that stops it.
    Uncaptured { chain_step: Option<String> },
    Captured {
        /// Why the results can be partial, so quarantine cannot excuse a
        /// failure.
        partial: Option<String>,
        /// Why a failing exit code cannot be read as the runner's "tests
        /// failed" code, so quarantine cannot excuse a failure.
        exit_code: Option<String>,
        /// Why a failed test cannot be retried.
        unretryable: Option<String>,
    },
}

/// Audit one command of a target. `plugin_name` is the project's plugin.
pub(crate) fn audit(
    plugin: &dyn LanguagePlugin,
    declared: Option<&ReportConfig>,
    ctx: &ReportContext,
    wrappers: &Wrappers,
    new_dir: &mut dyn FnMut() -> Option<PathBuf>,
) -> Audit {
    let steps = match test_reports::chain_for_target(plugin, declared, ctx, wrappers, new_dir) {
        Chain::Plan(chain) => {
            let steps = chain
                .steps
                .iter()
                .filter_map(|step| {
                    step.plan.as_ref().map(|(plan, _)| Step {
                        command: step.leaf.clone(),
                        plan: plan.clone(),
                    })
                })
                .collect::<Vec<_>>();
            chain.remove_report_dirs();
            (steps, true)
        }
        Chain::Unplanned { step } => {
            return Audit::Uncaptured {
                chain_step: Some(step),
            }
        }
        Chain::Single => match test_reports::plan(plugin, declared, ctx, wrappers) {
            Some(plan) => (
                vec![Step {
                    command: ctx.command.to_string(),
                    plan,
                }],
                false,
            ),
            None => return Audit::Uncaptured { chain_step: None },
        },
    };
    let (steps, chained) = steps;
    let mut partial = None;
    let mut exit_code = None;
    let mut unretryable = None;
    for step in &steps {
        let name = if step.plan.declared {
            step.plan.format.declared_capture_name()
        } else {
            step.plan.format.capture_name()
        };
        if partial.is_none() && step.plan.confidence != Confidence::Complete {
            partial = Some(format!("`{}`: {}", step.command, why_partial(&step.plan)));
        }
        if exit_code.is_none() && tests_failed_codes(plugin.name(), name, &step.command).is_empty()
        {
            exit_code = Some(format!(
                "`{}`: {}",
                step.command,
                why_no_exit_code(plugin.name(), &step.plan)
            ));
        }
        if unretryable.is_none() {
            // A chain step is retried on its own, without the wrapper.
            let no_wrappers = Wrappers::default();
            let wrappers = if chained { &no_wrappers } else { wrappers };
            if let Err(why) = retryable(ctx, wrappers, &step.command, plugin.name()) {
                unretryable = Some(format!("`{}`: {why}", step.command));
            }
        }
    }
    Audit::Captured {
        partial,
        exit_code,
        unretryable,
    }
}

fn why_partial(plan: &ReportPlan) -> String {
    match plan.format {
        ReportFormat::NodeTest => "the command only mentions `node --test` (a shell chain or a \
            script file), so it may run tests another way too"
            .to_string(),
        ReportFormat::ExUnit => "the project configures its own ExUnit formatters, so only the \
            failures in the console output are read"
            .to_string(),
        ReportFormat::LibtestText if plan.declared => {
            "a declared `libtest` report is saved console output".to_string()
        }
        ReportFormat::JUnitXml if matches!(&plan.files, ReportFiles::Paths(paths) if paths.is_empty()) => {
            "the command writes its own JUnit report to a path Aster cannot resolve".to_string()
        }
        _ => "the report may miss tests".to_string(),
    }
}

fn why_no_exit_code(plugin: &str, plan: &ReportPlan) -> String {
    if matches!(plugin, "gradle" | "maven") {
        "Gradle and Maven exit 1 for compile errors too".to_string()
    } else if plan.declared {
        "the command is a script, so its exit code says nothing certain about a runner's test \
         failures (put the runner command in the target)"
            .to_string()
    } else {
        "Aster cannot tell this runner's test failures from other errors by its exit code"
            .to_string()
    }
}

/// Whether Aster could rerun a failed test of `command`: a hypothetical
/// failure is narrowed by the runner's own selection flags.
fn retryable(
    ctx: &ReportContext,
    wrappers: &Wrappers,
    command: &str,
    plugin: &str,
) -> Result<(), String> {
    let failure = TestCase {
        file: Some("tests/a_test.py".to_string()),
        suite: Some("Suite".to_string()),
        name: "a test".to_string(),
        outcome: TestOutcome::Fail,
        attempt: 1,
        duration_ms: None,
        message: None,
    };
    if test_retry::commands(ctx.working_dir, command, &[&failure], wrappers).is_some() {
        return Ok(());
    }
    if matches!(plugin, "gradle" | "maven") {
        return Err("Gradle and Maven cannot select single tests".to_string());
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let cargo = words.contains(&"cargo");
    if cargo && words.contains(&"test") && !words.contains(&"--no-fail-fast") {
        return Err(
            "`cargo test` stops at the first failing test binary, so the tests after \
             it are unknown; add `--no-fail-fast`"
                .to_string(),
        );
    }
    if cargo && words.contains(&"nextest") && !words.contains(&"--no-fail-fast") {
        return Err("`cargo nextest` stops at the first failure; add `--no-fail-fast`".to_string());
    }
    Err(
        "Aster cannot build a per-test rerun from this command: a flag that stops at the \
         first failure (`-x`, `--bail`, `--max-failures`, `-failfast`), a selection it cannot \
         extend, or a runner or script it does not narrow"
            .to_string(),
    )
}
