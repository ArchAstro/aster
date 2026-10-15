//! The three record kinds in a results stream.

use serde::{Deserialize, Serialize};

/// One line of a results stream, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Run(RunRecord),
    Target(TargetRecord),
    Test(TestRecord),
}

/// Where and what one Aster invocation ran. Always the first record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Stable across re-uploads of the same invocation, e.g.
    /// `gh:<repo>:<run id>:<attempt>:<job>:<lane>`. The service ingests a
    /// run id at most once.
    pub run_id: String,
    /// `owner/name`.
    pub repo: String,
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// CI attempt of the whole run (GitHub's `run_attempt`), not a test retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    pub aster_version: String,
    /// RFC 3339.
    pub started_at: String,
}

/// How a target finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    Passed,
    Failed,
    Skipped,
}

/// How much of a target's test results Aster could see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A structured report covered every test the runner ran.
    Complete,
    /// Results came from parsing console output and may miss tests. Never
    /// enough to quarantine a failure.
    Partial,
}

/// One target in the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetRecord {
    /// `//path/to/project:target`.
    pub address: String,
    /// Content hash of the target's inputs. Two runs with the same hash and
    /// different test outcomes prove a test is flaky.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<String>,
    pub status: TargetStatus,
    #[serde(default)]
    pub cached: bool,
    pub duration_ms: u64,
    /// Which capture strategy produced the tests, e.g. `junit`, `go-json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// How one test attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Pass,
    /// An assertion failed.
    Fail,
    /// The test raised or crashed before an assertion decided it.
    Error,
    Skip,
}

impl TestOutcome {
    /// Whether this outcome fails the test.
    pub fn is_failure(self) -> bool {
        matches!(self, TestOutcome::Fail | TestOutcome::Error)
    }
}

/// One attempt of one test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestRecord {
    /// Address of the target that ran the test.
    pub target: String,
    /// [`crate::TestIdentity::test_id`].
    pub test_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    pub name: String,
    pub outcome: TestOutcome,
    /// 1 for the first attempt, 2 for the first retry, and so on.
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// [`crate::failure_hash`] of the failure message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_hash: Option<String>,
    /// [`crate::scrub_message`] of the failure message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Set on a test's last attempt when it failed but the test is
    /// quarantined and Aster let its target pass.
    #[serde(default, skip_serializing_if = "is_false")]
    pub quarantined: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(record: &Record) -> Record {
        let json = serde_json::to_string(record).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn run_record_round_trips_and_is_tagged() {
        let record = Record::Run(RunRecord {
            run_id: "gh:ArchAstro/firstlanding:18233412:1:test-ci:services-platform".into(),
            repo: "ArchAstro/firstlanding".into(),
            sha: "9f1c".into(),
            branch: Some("main".into()),
            pr: None,
            workflow: Some("CI".into()),
            job: Some("test-ci".into()),
            attempt: Some(1),
            lane: Some("services-platform".into()),
            runner: Some("blacksmith-8vcpu-ubuntu-2404".into()),
            aster_version: "0.18.0".into(),
            started_at: "2026-10-13T17:02:11Z".into(),
        });
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["type"], "run");
        assert!(json.get("pr").is_none(), "absent fields are omitted");
        assert_eq!(round_trip(&record), record);
    }

    #[test]
    fn target_record_round_trips() {
        let record = Record::Target(TargetRecord {
            address: "//services/platform:test-ci".into(),
            input_hash: Some("sha256:4be2".into()),
            status: TargetStatus::Failed,
            cached: false,
            duration_ms: 412_233,
            capture: Some("exunit-formatter".into()),
            confidence: Some(Confidence::Complete),
            exit_code: Some(2),
        });
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["type"], "target");
        assert_eq!(json["status"], "failed");
        assert_eq!(json["confidence"], "complete");
        assert_eq!(round_trip(&record), record);
    }

    #[test]
    fn test_record_round_trips() {
        let record = Record::Test(TestRecord {
            target: "//services/platform:test-ci".into(),
            test_id: "c81e".into(),
            file: Some("test/platform/agents_test.exs".into()),
            suite: Some("Platform.AgentsTest".into()),
            name: "test creates an agent".into(),
            outcome: TestOutcome::Fail,
            attempt: 1,
            duration_ms: Some(1840),
            failure_hash: Some("f3a9".into()),
            message: Some("** (DBConnection.OwnershipError) cannot find owner".into()),
            quarantined: true,
        });
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["type"], "test");
        assert_eq!(json["outcome"], "fail");
        assert_eq!(json["quarantined"], true);
        assert_eq!(round_trip(&record), record);
        // Only a quarantined failure says so; older readers ignore it.
        let Record::Test(mut plain) = record else {
            unreachable!()
        };
        plain.quarantined = false;
        let json = serde_json::to_value(Record::Test(plain)).unwrap();
        assert!(json.get("quarantined").is_none());
    }

    #[test]
    fn passing_test_needs_only_the_essentials() {
        let line = r#"{"type":"test","target":"//a:test","test_id":"x","name":"t","outcome":"pass","attempt":1}"#;
        let record: Record = serde_json::from_str(line).unwrap();
        let Record::Test(test) = record else {
            panic!("expected a test record")
        };
        assert_eq!(test.outcome, TestOutcome::Pass);
        assert_eq!(test.duration_ms, None);
        assert!(!test.outcome.is_failure());
    }

    #[test]
    fn error_and_fail_are_failures() {
        assert!(TestOutcome::Fail.is_failure());
        assert!(TestOutcome::Error.is_failure());
        assert!(!TestOutcome::Skip.is_failure());
    }
}
