//! End-to-end tests for `--test-results`: per-test capture from a native
//! runner's report, the NDJSON results file, and the `--json` summary.
//!
//! The fixture is a Gradle project whose `test` command is replaced by a
//! shell script that writes the JUnit XML Gradle would, so the tests need no
//! Gradle installation.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use aster_results_model::{parse_ndjson, Confidence, Record, TargetStatus, TestOutcome};
use tempfile::TempDir;

const REPORT: &str = r#"<testsuite name="com.example.CalculatorTest"><testcase name="adds()" classname="com.example.CalculatorTest" time="0.004"/><testcase name="divides()" classname="com.example.CalculatorTest" time="0.027"><failure message="expected: 2 but was: 3">at CalculatorTest.java:14</failure></testcase></testsuite>"#;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(tmp.path(), "app/build.gradle", "apply plugin: 'java'\n");
    write(
        tmp.path(),
        "app/src/test/java/CalculatorTest.java",
        "class CalculatorTest {}\n",
    );
    fs::write(tmp.path().join("report.xml"), REPORT).unwrap();
    write(
        tmp.path(),
        "app/aster.toml",
        r#"
[targets.deps]
command = "true"

[targets.test]
command = "sh -c 'mkdir -p build/test-results/test && cp ../report.xml build/test-results/test/TEST-CalculatorTest.xml && exit 1'"
"#,
    );
    tmp
}

fn aster(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(root)
        .env_remove("ASTER_TEST_RESULTS")
        .env_remove("GITHUB_RUN_ID")
        .env_remove("ASTER_RETRY_FAILED")
        .env("RUNNER_TEMP", root.join("runner-temp"))
        .args(args)
        .output()
        .unwrap()
}

fn read_records(path: &Path) -> Vec<Record> {
    parse_ndjson(&fs::read_to_string(path).unwrap()).unwrap()
}

fn test_target_hash(records: &[Record]) -> Option<String> {
    records.iter().find_map(|record| match record {
        Record::Target(target) if target.address == "//app:test" => target.input_hash.clone(),
        _ => None,
    })
}

#[test]
fn records_each_test_to_the_results_file_and_json() {
    let tmp = workspace();
    let results = tmp.path().join("out/results.ndjson");
    let output = aster(
        tmp.path(),
        &[
            "--json",
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    assert!(!output.status.success(), "the failing test fails the run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Test results (2 tests) written to"),
        "{stderr}"
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let test = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["address"] == "//app:test")
        .unwrap();
    assert_eq!(test["tests"]["capture"], "junit");
    assert_eq!(test["tests"]["confidence"], "complete");
    assert_eq!(test["tests"]["passed"], 1);
    assert_eq!(test["tests"]["failed"], 1);
    assert_eq!(test["tests"]["failures"][0]["name"], "divides()");

    let records = read_records(&results);
    let Record::Run(run) = &records[0] else {
        panic!("the run record comes first")
    };
    assert!(run.run_id.starts_with("local:test:"), "{}", run.run_id);
    let target = records
        .iter()
        .find_map(|r| match r {
            Record::Target(t) if t.address == "//app:test" => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(target.status, TargetStatus::Failed);
    assert_eq!(target.confidence, Some(Confidence::Complete));
    assert_eq!(target.exit_code, Some(1));
    assert!(target.input_hash.is_some());
    let tests: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            Record::Test(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[1].name, "divides()");
    assert_eq!(
        tests[1].suite.as_deref(),
        Some("com.example.CalculatorTest")
    );
    assert_eq!(tests[1].outcome, TestOutcome::Fail);
    assert!(tests[1].failure_hash.is_some());
    assert!(tests[1]
        .message
        .as_deref()
        .unwrap()
        .starts_with("expected: 2 but was: 3"));
}

#[test]
fn input_hash_is_stable_until_a_test_input_changes() {
    let tmp = workspace();
    let run = |name: &str| {
        let path = tmp.path().join(format!("{name}.ndjson"));
        aster(
            tmp.path(),
            &[
                "--quiet",
                &format!("--test-results={}", path.display()),
                "test",
                "//app",
            ],
        );
        test_target_hash(&read_records(&path)).expect("the test target is hashed")
    };
    let first = run("first");
    assert_eq!(run("second"), first, "same inputs, same hash");
    write(
        tmp.path(),
        "app/src/test/java/CalculatorTest.java",
        "class CalculatorTest { void more() {} }\n",
    );
    assert_ne!(run("third"), first, "a changed test file changes the hash");
}

#[test]
fn default_location_is_outside_the_checkout() {
    let tmp = workspace();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_TEST_RESULTS", "1")
        .env_remove("GITHUB_RUN_ID")
        .env("RUNNER_TEMP", tmp.path().join("runner-temp"))
        .args(["--quiet", "test", "//app"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let written = stderr
        .lines()
        .find_map(|line| line.split(" written to ").nth(1))
        .unwrap_or_else(|| panic!("no results path in: {stderr}"));
    assert!(
        Path::new(written).starts_with(tmp.path().join("runner-temp/aster-results")),
        "{written}"
    );
    assert!(read_records(Path::new(written)).len() > 2);
    // Nothing is left in the project but what the test command wrote.
    assert!(!tmp.path().join(".aster/results").exists());
}

#[test]
fn without_the_flag_nothing_is_recorded() {
    let tmp = workspace();
    let output = aster(tmp.path(), &["--json", "test", "//app"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.get("tests").is_none()));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Test results"));
    assert!(!tmp.path().join("runner-temp/aster-results").exists());
}

// --retry-failed, end to end with a real `go test`.

fn go_available() -> bool {
    Command::new("go").arg("version").output().is_ok()
}

/// A Go module whose `TestFlaky` fails until `../marker` exists (it creates
/// the file as it fails) and whose `TestStable` logs each run to
/// `../stable.log`, so the log shows what was rerun.
fn flaky_go_workspace(test_target: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(
        tmp.path(),
        "app/go.mod",
        "module example.com/app\n\ngo 1.21\n",
    );
    write(
        tmp.path(),
        "app/app_test.go",
        r#"package app

import (
	"fmt"
	"os"
	"testing"
)

func TestStable(t *testing.T) {
	f, _ := os.OpenFile("../stable.log", os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	fmt.Fprintln(f, "ran")
	f.Close()
}

func TestFlaky(t *testing.T) {
	if _, err := os.Stat("../marker"); err != nil {
		os.WriteFile("../marker", []byte("x"), 0o644)
		t.Fatal("fails on the first run")
	}
}
"#,
    );
    write(
        tmp.path(),
        "app/aster.toml",
        &format!("[targets.deps]\ncommand = \"true\"\n\n[targets.test]\n{test_target}\n"),
    );
    tmp
}

fn tests_of(records: &[Record]) -> Vec<(String, TestOutcome, u32)> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::Test(test) => Some((test.name.clone(), test.outcome, test.attempt)),
            _ => None,
        })
        .collect()
}

fn target_status(records: &[Record], address: &str) -> TargetStatus {
    records
        .iter()
        .find_map(|r| match r {
            Record::Target(t) if t.address == address => Some(t.status),
            _ => None,
        })
        .unwrap()
}

#[test]
fn retry_failed_reruns_only_the_failing_test_and_marks_the_pass_flaky() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = flaky_go_workspace(
        "command = \"go test ./...\"\nretry_setup = \"echo reset >> ../setup.log\"",
    );
    let results = tmp.path().join("out/results.ndjson");
    let output = aster(
        tmp.path(),
        &[
            "--no-cache",
            "--retry-failed=1",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "the retry passes: {stderr}");
    assert!(
        stderr.contains("PASS //app:test") && stderr.contains("flaky: 1 test passed on retry"),
        "{stderr}"
    );

    let records = read_records(&results);
    assert_eq!(target_status(&records, "//app:test"), TargetStatus::Passed);
    let tests = tests_of(&records);
    for expected in [
        ("TestFlaky", TestOutcome::Fail, 1),
        ("TestFlaky", TestOutcome::Pass, 2),
        ("TestStable", TestOutcome::Pass, 1),
    ] {
        assert!(
            tests.contains(&(expected.0.into(), expected.1, expected.2)),
            "{expected:?} in {tests:?}"
        );
    }
    assert_eq!(tests.len(), 3, "{tests:?}");

    // TestStable ran once: the retry selected only TestFlaky.
    let stable = fs::read_to_string(tmp.path().join("stable.log")).unwrap();
    assert_eq!(stable.lines().count(), 1, "{stable}");
    // The setup ran once, before the retry.
    let setup = fs::read_to_string(tmp.path().join("setup.log")).unwrap();
    assert_eq!(setup.lines().count(), 1, "{setup}");
}

#[test]
fn retry_failed_lists_the_flaky_tests_in_json_without_a_results_file() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = flaky_go_workspace("command = \"go test ./...\"");
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env_remove("ASTER_TEST_RESULTS")
        .env_remove("GITHUB_RUN_ID")
        .env("ASTER_RETRY_FAILED", "2")
        .env("GITHUB_ACTIONS", "true")
        .env("RUNNER_TEMP", tmp.path().join("runner-temp"))
        .args(["--json", "--no-cache", "test", "//app"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("::warning file=app/app_test.go::TestFlaky failed then passed on retry")
            || stderr.contains("::warning::TestFlaky failed then passed on retry"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Test results"),
        "no results file was asked for: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let test = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["address"] == "//app:test")
        .unwrap();
    assert_eq!(test["status"], "passed");
    assert_eq!(test["tests"]["flaky"][0]["name"], "TestFlaky");
    assert_eq!(test["tests"]["flaky"][0]["passed_on_attempt"], 2);
}

#[test]
fn a_failing_retry_setup_skips_the_retry_and_keeps_the_failure() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = flaky_go_workspace("command = \"go test ./...\"\nretry_setup = \"exit 3\"");
    let results = tmp.path().join("results.ndjson");
    let output = aster(
        tmp.path(),
        &[
            "--no-cache",
            "--retry-failed",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    assert!(!output.status.success());
    let records = read_records(&results);
    let tests = tests_of(&records);
    assert!(
        tests.contains(&("TestFlaky".into(), TestOutcome::Fail, 1)),
        "{tests:?}"
    );
    assert!(
        tests.iter().all(|(_, _, attempt)| *attempt == 1),
        "{tests:?}"
    );
    assert_eq!(target_status(&records, "//app:test"), TargetStatus::Failed);
}

#[test]
fn a_pass_that_needed_a_retry_is_not_cached() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = flaky_go_workspace("command = \"go test ./...\"");
    let run = || aster(tmp.path(), &["--retry-failed", "test", "//app"]);
    let first = run();
    assert!(first.status.success());
    assert!(String::from_utf8_lossy(&first.stderr).contains("flaky: 1 test passed on retry"));
    // The flake comes back, and the target runs again instead of replaying a
    // cached pass.
    fs::remove_file(tmp.path().join("marker")).unwrap();
    let second = run();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "{stderr}");
    assert!(stderr.contains("flaky: 1 test passed on retry"), "{stderr}");
}

#[test]
fn without_retry_failed_a_failing_test_fails_the_target() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = flaky_go_workspace("command = \"go test ./...\"");
    let output = aster(tmp.path(), &["--no-cache", "test", "//app"]);
    assert!(!output.status.success());
}
