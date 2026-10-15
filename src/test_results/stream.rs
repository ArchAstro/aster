//! Turning execution results into a results stream.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use aster_results_model::{
    failure_hash, scrub_message, Record, RunRecord, TargetRecord, TargetStatus, TestIdentity,
    TestRecord,
};

use crate::executor::ExecutionResult;

/// The records for one invocation: the run, then each target followed by
/// its tests.
pub fn records(run: RunRecord, results: &[ExecutionResult]) -> Vec<Record> {
    let mut records = vec![Record::Run(run)];
    for result in results {
        let status = if result.skipped {
            TargetStatus::Skipped
        } else if result.success {
            TargetStatus::Passed
        } else {
            TargetStatus::Failed
        };
        records.push(Record::Target(TargetRecord {
            address: result.address.clone(),
            input_hash: result.input_hash.clone(),
            status,
            cached: result.cached,
            duration_ms: u64::try_from(result.duration_ms).unwrap_or(u64::MAX),
            capture: result.tests.as_ref().map(|t| t.capture.clone()),
            confidence: result.tests.as_ref().map(|t| t.confidence),
            exit_code: result.exit_code,
        }));
        let Some(report) = &result.tests else {
            continue;
        };
        for case in &report.cases {
            // The last attempt of a test the quarantine let through.
            let quarantined = case.outcome.is_failure()
                && report
                    .quarantined
                    .iter()
                    .any(|q| q.name == case.name && q.suite == case.suite && q.file == case.file)
                && !report
                    .cases
                    .iter()
                    .any(|other| super::same_test(other, case) && other.attempt > case.attempt);
            let message = case
                .message
                .as_deref()
                .filter(|_| case.outcome.is_failure());
            records.push(Record::Test(TestRecord {
                target: result.address.clone(),
                test_id: test_id(&result.address, report.project.as_deref(), case),
                file: case.file.clone(),
                suite: case.suite.clone(),
                name: case.name.clone(),
                outcome: case.outcome,
                attempt: case.attempt,
                duration_ms: case.duration_ms,
                failure_hash: message.map(failure_hash),
                message: message.map(scrub_message),
                quarantined,
            }));
        }
    }
    records
}

/// The test id of `case`, run by the target at `address`: its project,
/// file, suite and name. The project is `project` when the report names one
/// ([`super::TestReport::project`]), else the target's own.
pub(crate) fn test_id(address: &str, project: Option<&str>, case: &super::TestCase) -> String {
    let project = project.unwrap_or_else(|| {
        address
            .rsplit_once(':')
            .map_or(address, |(project, _)| project)
    });
    TestIdentity::new(
        project,
        case.file.clone().unwrap_or_default(),
        case.suite.clone().unwrap_or_default(),
        case.name.clone(),
    )
    .test_id()
}

/// Where a results file goes when no path is given: under `$RUNNER_TEMP`
/// when set, else the system temp directory, never in the checkout.
pub fn default_path(run_id: &str) -> PathBuf {
    let base = std::env::var_os("RUNNER_TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let name: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    base.join("aster-results").join(format!("{name}.ndjson"))
}

/// Write the records as NDJSON, creating parent directories.
pub fn write(path: &Path, records: &[Record]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(path, aster_results_model::to_ndjson(records))
        .with_context(|| format!("cannot write test results to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_results::{Confidence, TestCase, TestOutcome, TestReport};

    fn run() -> RunRecord {
        RunRecord {
            run_id: "gh:a/b:1:1:test:test:-:00".into(),
            repo: "a/b".into(),
            sha: "abc".into(),
            branch: None,
            pr: None,
            workflow: None,
            job: None,
            attempt: None,
            lane: None,
            runner: None,
            aster_version: "0".into(),
            started_at: "2026-10-13T00:00:00Z".into(),
        }
    }

    fn result(address: &str, success: bool, tests: Option<TestReport>) -> ExecutionResult {
        ExecutionResult {
            address: address.into(),
            success,
            skipped: false,
            cached: false,
            output: String::new(),
            log: Vec::new(),
            duration_ms: 1200,
            exit_code: Some(if success { 0 } else { 2 }),
            tests,
            input_hash: Some("h".into()),
        }
    }

    fn case(name: &str, outcome: TestOutcome, message: Option<&str>) -> TestCase {
        TestCase {
            file: Some("test/a_test.exs".into()),
            suite: Some("AppTest".into()),
            name: name.into(),
            outcome,
            attempt: 1,
            duration_ms: Some(5),
            message: message.map(str::to_string),
        }
    }

    #[test]
    fn marks_the_last_attempt_of_a_quarantined_failure() {
        let mut retried = case("flaky", TestOutcome::Fail, Some("boom"));
        retried.attempt = 2;
        let report = TestReport {
            project: None,
            capture: "exunit".into(),
            confidence: Confidence::Complete,
            cases: vec![
                case("flaky", TestOutcome::Fail, Some("boom")),
                retried,
                case("other", TestOutcome::Fail, Some("boom")),
            ],
            quarantined: vec![crate::test_results::quarantine::Quarantined {
                name: "flaky".into(),
                suite: Some("AppTest".into()),
                file: Some("test/a_test.exs".into()),
                source: "aster-srv".into(),
            }],
        };
        let records = records(run(), &[result("//app:test", true, Some(report))]);
        let marks: Vec<(String, u32, bool)> = records
            .iter()
            .filter_map(|r| match r {
                Record::Test(t) => Some((t.name.clone(), t.attempt, t.quarantined)),
                _ => None,
            })
            .collect();
        assert_eq!(
            marks,
            [
                ("flaky".to_string(), 1, false),
                ("flaky".to_string(), 2, true),
                ("other".to_string(), 1, false)
            ]
        );
    }

    #[test]
    fn writes_run_then_targets_then_their_tests() {
        let report = TestReport {
            project: None,
            capture: "junit".into(),
            confidence: Confidence::Complete,
            cases: vec![
                case("passes", TestOutcome::Pass, None),
                case(
                    "fails",
                    TestOutcome::Fail,
                    Some("boom at line 12 with Bearer abcdefghijklmnop"),
                ),
            ],
            quarantined: Vec::new(),
        };
        let records = records(
            run(),
            &[
                result("//services/app:test", false, Some(report)),
                result("//services/app:deps", true, None),
            ],
        );
        assert!(matches!(records[0], Record::Run(_)));
        let Record::Target(target) = &records[1] else {
            panic!("expected target")
        };
        assert_eq!(target.address, "//services/app:test");
        assert_eq!(target.status, TargetStatus::Failed);
        assert_eq!(target.capture.as_deref(), Some("junit"));
        assert_eq!(target.confidence, Some(Confidence::Complete));
        assert_eq!(target.input_hash.as_deref(), Some("h"));
        assert_eq!(target.exit_code, Some(2));

        let Record::Test(passing) = &records[2] else {
            panic!("expected test")
        };
        assert_eq!(passing.message, None);
        assert_eq!(passing.failure_hash, None);
        let Record::Test(failing) = &records[3] else {
            panic!("expected test")
        };
        let identity = TestIdentity::new("//services/app", "test/a_test.exs", "AppTest", "fails");
        assert_eq!(failing.test_id, identity.test_id());
        assert_eq!(
            failing.message.as_deref(),
            Some("boom at line 12 with Bearer [redacted]")
        );
        assert_eq!(
            failing.failure_hash.as_deref(),
            Some(failure_hash("boom at line 99 with Bearer abcdefghijklmnop").as_str())
        );

        assert!(matches!(&records[4], Record::Target(t) if t.capture.is_none()));
        assert_eq!(records.len(), 5);
    }

    #[test]
    fn a_shards_tests_are_named_for_the_project_that_owns_them() {
        let report = |shard: &str| {
            let report = TestReport {
                project: Some("//services/platform".into()),
                capture: "exunit-formatter".into(),
                confidence: Confidence::Complete,
                cases: vec![case("fails", TestOutcome::Fail, Some("boom"))],
                quarantined: Vec::new(),
            };
            records(run(), &[result(shard, false, Some(report))])
        };
        let ids: Vec<String> = [
            "//services/platform-test-shard-1:test-ci",
            "//services/platform-test-shard-3:test-ci",
        ]
        .into_iter()
        .map(|shard| match &report(shard)[2] {
            Record::Test(test) => {
                assert_eq!(test.target, shard);
                test.test_id.clone()
            }
            _ => panic!("expected test"),
        })
        .collect();
        let owned = TestIdentity::new("//services/platform", "test/a_test.exs", "AppTest", "fails");
        assert_eq!(ids, [owned.test_id(), owned.test_id()]);
    }

    #[test]
    fn writes_ndjson_that_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/results.ndjson");
        let records = records(run(), &[result("//a:test", true, None)]);
        write(&path, &records).unwrap();
        let read =
            aster_results_model::parse_ndjson(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, records);
    }

    #[test]
    fn default_path_is_a_safe_file_name_outside_the_checkout() {
        let path = default_path("gh:a/b:1:1:test-ci:x y");
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "gh_a_b_1_1_test-ci_x_y.ndjson"
        );
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "aster-results");
    }
}
