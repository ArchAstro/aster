//! `--retry-failed`: rerunning only the tests that failed.
//!
//! A failed target whose captured report names failing tests is rerun with
//! the runner's own selection flags, up to a limit, and every attempt is
//! recorded in the report with its attempt number. The target passes only
//! when every failing test passes on some retry, nothing else fails, and
//! every retry command exits 0.

use super::*;
use crate::plugins::test_retry;
use crate::test_results::same_test;
use crate::test_results::{TestCase, TestOutcome};

/// What a retry needs to know about the target it reruns.
pub(super) struct RetryJob<'a> {
    pub address: &'a str,
    pub plugin: &'a dyn LanguagePlugin,
    pub project_dir: &'a Path,
    pub working_dir: &'a Path,
    pub target_name: &'a str,
    /// The target's one command, as written.
    pub command: &'a str,
    /// Run in the working directory before each retry.
    pub setup: Option<&'a str>,
    pub max_retries: u32,
    pub null_stdin: bool,
    pub log_cap: usize,
}

/// Rerun the failing tests of a failed `result`, updating it in place:
/// success, exit code, duration, log and the report with every attempt.
/// Returns whether any retry ran.
///
/// Nothing is retried when the failure is not attributable to tests: no
/// failing test in the report, a command killed by a signal, or a runner
/// that cannot select single tests. A failing `retry_setup` skips its
/// retry and keeps the failure as it stood.
pub(super) fn rerun_failures(job: &RetryJob, result: &mut ExecutionResult) -> bool {
    if result.success
        || result.skipped
        || !matches!(result.exit_code, Some(1..=128))
        || job.max_retries == 0
    {
        return false;
    }
    let Some(report) = result.tests.clone() else {
        return false;
    };
    let mut failing = unique_failures(&report.cases);
    if failing.is_empty() {
        return false;
    }

    let mut cases = report.cases;
    let mut confidence = report.confidence;
    let mut ran_any = false;
    for round in 2..=job.max_retries + 1 {
        if shutdown_requested() {
            break;
        }
        let refs: Vec<&TestCase> = failing.iter().collect();
        let Some(commands) = test_retry::commands(job.working_dir, job.command, &refs) else {
            break;
        };
        let mut captures = Vec::new();
        for command in &commands {
            captures.push(plan_captures(
                job.plugin,
                job.project_dir,
                job.working_dir,
                job.target_name,
                std::slice::from_ref(command),
            ));
        }
        if captures.iter().any(|c| c.iter().all(Option::is_none)) {
            remove_report_dirs(&captures);
            break;
        }

        let note = format!(
            "[aster] Retry {}/{}: rerunning {} failing test{}\n",
            round - 1,
            job.max_retries,
            failing.len(),
            if failing.len() == 1 { "" } else { "s" }
        );
        if let Some(setup) = job.setup {
            let setup_result = run_setup(job, setup);
            if !setup_result.success {
                append_log(
                    result,
                    &format!(
                        "{note}[aster] retry_setup failed (exit {}); not retrying\n",
                        setup_result
                            .exit_code
                            .map_or("?".to_string(), |c| c.to_string())
                    ),
                    &setup_result.log,
                );
                remove_report_dirs(&captures);
                break;
            }
        }
        append_log(result, &note, &[]);

        let mut retried: Vec<TestCase> = Vec::new();
        let mut commands_ok = true;
        let mut last_exit = None;
        for (command, capture) in commands.iter().zip(&captures) {
            let invocation = match capture.first() {
                Some(Some((plan, _))) => Invocation::planned(command, plan),
                _ => Invocation::plain(command),
            };
            let started = SystemTime::now();
            let (ran, raw_outputs) = run_invocations(
                job.address,
                std::slice::from_ref(&invocation),
                job.working_dir,
                job.null_stdin,
                job.log_cap,
            );
            ran_any = true;
            commands_ok &= ran.success;
            last_exit = ran.exit_code;
            result.duration_ms += ran.duration_ms;
            append_log(result, &format!("$ {command}\n"), &ran.log);
            match read_test_report(job.address, capture, &raw_outputs, started) {
                Some(found) => {
                    if found.confidence != Confidence::Complete {
                        confidence = Confidence::Partial;
                    }
                    retried.extend(found.cases);
                }
                None => confidence = Confidence::Partial,
            }
        }
        result.exit_code = last_exit;

        // Tests that were selected, and any others that failed.
        let still_failing = merge_attempt(&mut cases, retried, &failing, round);
        if still_failing.is_empty() {
            result.success = commands_ok;
            break;
        }
        // A retry that failed and named no test again would loop on a
        // failure the report cannot explain; keep going only while it can.
        failing = still_failing;
    }

    if ran_any {
        result.tests = Some(TestReport {
            project: report.project,
            capture: report.capture,
            confidence,
            cases,
        });
    }
    ran_any
}

/// The failing tests of the latest attempt of each test, once each.
fn unique_failures(cases: &[TestCase]) -> Vec<TestCase> {
    let mut failing: Vec<TestCase> = Vec::new();
    for case in cases.iter().filter(|c| c.outcome.is_failure()) {
        let latest = cases
            .iter()
            .filter(|other| same_test(other, case))
            .map(|other| other.attempt)
            .max();
        if latest != Some(case.attempt) {
            continue;
        }
        if !failing.iter().any(|known| same_test(known, case)) {
            failing.push(case.clone());
        }
    }
    failing
}

/// Record a retry's cases as attempt `round` of the tests they rerun
/// (numbered after the attempts already recorded for each test). Cases of
/// tests that were neither selected nor failing are not recorded. Returns
/// the tests that still fail: selected tests that did not pass, and any
/// other that failed.
fn merge_attempt(
    cases: &mut Vec<TestCase>,
    retried: Vec<TestCase>,
    selected: &[TestCase],
    round: u32,
) -> Vec<TestCase> {
    let mut still_failing: Vec<TestCase> = Vec::new();
    for case in &retried {
        if case.outcome.is_failure() && !still_failing.iter().any(|known| same_test(known, case)) {
            still_failing.push(case.clone());
        }
    }
    for test in selected {
        let passed = retried
            .iter()
            .any(|case| same_test(case, test) && case.outcome == TestOutcome::Pass);
        let failed_again = still_failing.iter().any(|known| same_test(known, test));
        if !passed && !failed_again {
            still_failing.push(test.clone());
        }
    }
    for mut case in retried {
        if !case.outcome.is_failure() && !selected.iter().any(|test| same_test(test, &case)) {
            continue;
        }
        let previous = cases
            .iter()
            .filter(|known| same_test(known, &case))
            .map(|known| known.attempt)
            .max();
        case.attempt = previous.map_or(round, |attempt| attempt + 1);
        cases.push(case);
    }
    still_failing
}

fn remove_report_dirs(captures: &[Vec<Option<(ReportPlan, PathBuf)>>]) {
    for (_, dir) in captures.iter().flatten().flatten() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Add a note and a command's log to the result's log.
fn append_log(result: &mut ExecutionResult, note: &str, log: &[u8]) {
    if !result.log.is_empty() && !result.log.ends_with(b"\n") {
        result.log.push(b'\n');
    }
    result.log.extend_from_slice(note.as_bytes());
    result.log.extend_from_slice(log);
    result.output = String::from_utf8_lossy(&result.log).into_owned();
}

/// Run a target's `retry_setup` in a shell.
fn run_setup(job: &RetryJob, setup: &str) -> ExecutionResult {
    let quoted = crate::executor::quote_command_argument(setup);
    #[cfg(windows)]
    let command = format!("cmd /C {quoted}");
    #[cfg(not(windows))]
    let command = format!("sh -c {quoted}");
    run_command(
        job.address,
        &command,
        &[],
        job.working_dir,
        job.null_stdin,
        job.log_cap,
    )
}

/// The GitHub Actions annotations for flaky tests: one warning per test,
/// on its file when the workspace-relative path is known.
pub(super) fn flaky_annotations(
    report: &TestReport,
    working_dir: &Path,
    workspace_root: &Path,
) -> Vec<String> {
    report
        .flaky()
        .iter()
        .map(|test| {
            let message = escape_data(&format!("{} failed then passed on retry", test.name));
            match &test.file {
                Some(file) => {
                    let absolute = working_dir.join(file);
                    let path = absolute
                        .strip_prefix(workspace_root)
                        .unwrap_or(Path::new(file))
                        .to_string_lossy()
                        .replace('\\', "/");
                    format!("::warning file={}::{message}", escape_property(&path))
                }
                None => format!("::warning::{message}"),
            }
        })
        .collect()
}

fn escape_data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, outcome: TestOutcome, attempt: u32) -> TestCase {
        TestCase {
            file: Some("t.go".into()),
            suite: Some("pkg".into()),
            name: name.into(),
            outcome,
            attempt,
            duration_ms: None,
            message: None,
        }
    }

    fn failed_result(exit_code: Option<i32>, cases: Vec<TestCase>) -> ExecutionResult {
        ExecutionResult {
            address: "//app:test".into(),
            success: false,
            skipped: false,
            cached: false,
            output: String::new(),
            log: Vec::new(),
            duration_ms: 10,
            exit_code,
            tests: Some(TestReport {
                project: None,
                capture: "go-json".into(),
                confidence: Confidence::Complete,
                cases,
            }),
            input_hash: None,
        }
    }

    #[test]
    fn failures_that_are_not_tests_are_not_retried() {
        let registry = PluginRegistry::with_all_plugins();
        let plugin = registry.find_by_name("go").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let job = RetryJob {
            address: "//app:test",
            plugin,
            project_dir: tmp.path(),
            working_dir: tmp.path(),
            target_name: "test",
            command: "go test ./...",
            setup: None,
            max_retries: 1,
            null_stdin: true,
            log_cap: 1024,
        };
        // Killed by SIGKILL, though a test failed.
        let mut killed = failed_result(Some(137), vec![case("A", TestOutcome::Fail, 1)]);
        assert!(!rerun_failures(&job, &mut killed));
        assert!(!killed.success);
        // A crash or compile error: nothing in the report failed.
        let mut crashed = failed_result(Some(1), vec![case("A", TestOutcome::Pass, 1)]);
        assert!(!rerun_failures(&job, &mut crashed));
        // No report at all.
        let mut unreported = failed_result(Some(1), Vec::new());
        unreported.tests = None;
        assert!(!rerun_failures(&job, &mut unreported));
        // A runner that cannot select tests.
        let gradle = RetryJob {
            command: "gradle test",
            ..job
        };
        let mut result = failed_result(Some(1), vec![case("A", TestOutcome::Fail, 1)]);
        assert!(!rerun_failures(&gradle, &mut result));
        assert_eq!(result.tests.unwrap().cases.len(), 1);
    }

    #[test]
    fn a_retry_that_passes_resolves_the_failure() {
        let mut cases = vec![
            case("A", TestOutcome::Fail, 1),
            case("B", TestOutcome::Pass, 1),
        ];
        let selected = unique_failures(&cases);
        assert_eq!(selected.len(), 1);
        let still = merge_attempt(
            &mut cases,
            vec![case("A", TestOutcome::Pass, 1)],
            &selected,
            2,
        );
        assert!(still.is_empty());
        assert_eq!(cases.len(), 3);
        assert_eq!((cases[2].name.as_str(), cases[2].attempt), ("A", 2));
    }

    #[test]
    fn unselected_passes_are_not_recorded_but_new_failures_are() {
        let mut cases = vec![
            case("A", TestOutcome::Fail, 1),
            case("B", TestOutcome::Pass, 1),
        ];
        let selected = unique_failures(&cases);
        let still = merge_attempt(
            &mut cases,
            vec![
                case("A", TestOutcome::Pass, 1),
                case("B", TestOutcome::Fail, 1),
                case("C", TestOutcome::Pass, 1),
            ],
            &selected,
            2,
        );
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].name, "B");
        let recorded: Vec<_> = cases.iter().map(|c| (c.name.as_str(), c.attempt)).collect();
        assert_eq!(recorded, [("A", 1), ("B", 1), ("A", 2), ("B", 2)]);
    }

    #[test]
    fn a_selected_test_missing_from_the_retry_still_fails() {
        let mut cases = vec![case("A", TestOutcome::Fail, 1)];
        let selected = unique_failures(&cases);
        let still = merge_attempt(&mut cases, Vec::new(), &selected, 2);
        assert_eq!(still.len(), 1);
        assert_eq!(cases.len(), 1);
    }

    #[test]
    fn only_the_latest_attempt_of_a_test_is_retried() {
        let cases = vec![
            case("A", TestOutcome::Fail, 1),
            case("A", TestOutcome::Pass, 2),
            case("B", TestOutcome::Fail, 1),
        ];
        let failing = unique_failures(&cases);
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].name, "B");
    }

    #[test]
    fn annotations_name_the_file_and_escape_the_message() {
        let report = TestReport {
            project: None,
            capture: "go-json".into(),
            confidence: Confidence::Complete,
            cases: vec![
                case("TestA 100%", TestOutcome::Fail, 1),
                case("TestA 100%", TestOutcome::Pass, 2),
            ],
        };
        let lines = flaky_annotations(&report, Path::new("/ws/app"), Path::new("/ws"));
        assert_eq!(
            lines,
            ["::warning file=app/t.go::TestA 100%25 failed then passed on retry"]
        );
    }
}
