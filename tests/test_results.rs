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
    // A shim can spawn and still exit non-zero when no toolchain is selected.
    Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
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
fn a_runner_behind_a_configured_wrapper_is_recorded_and_retried() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    // `env` stands in for `mise exec go --`: a program that runs the
    // command after it.
    let run = |wrappers: &str| {
        let tmp = flaky_go_workspace("command = \"env go test ./...\"");
        write(tmp.path(), "aster.toml", wrappers);
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
        let records = if results.exists() {
            read_records(&results)
        } else {
            Vec::new()
        };
        (tmp, output, records)
    };

    // Before: Aster did not read through the wrapper, so the run recorded
    // no tests and the failing one could not be retried.
    let (_tmp, output, records) = run("");
    assert!(!output.status.success());
    assert!(tests_of(&records).is_empty(), "{records:?}");

    // After: the wrapper stays in front of the planned command, the
    // failing test is rerun alone, and every attempt is recorded.
    let (tmp, output, records) = run("[affected.related]\nwrappers = [\"env\"]\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "the retry passes: {stderr}");
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
    let stable = fs::read_to_string(tmp.path().join("stable.log")).unwrap();
    assert_eq!(stable.lines().count(), 1, "{stable}");
    assert!(stderr.contains("flaky: 1 test passed on retry"), "{stderr}");
}

/// A package script chain `node --test a && node check.js && node --test b
/// && node post.js`. `a` and `b` each hold a stable test and one that fails
/// until its marker file exists; the other steps leave a line in a log.
fn chain_workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(
        tmp.path(),
        "app/package.json",
        r#"{"name":"app","scripts":{"test-ci":"node --test a.test.js && node check.js && node --test b.test.js && node post.js"}}"#,
    );
    for name in ["a", "b"] {
        write(
            tmp.path(),
            &format!("app/{name}.test.js"),
            &format!(
                r#"const test = require('node:test');
const fs = require('node:fs');
test('stable {name}', () => {{ fs.appendFileSync('../stable-{name}.log', 'ran\n'); }});
test('flaky {name}', () => {{
  if (!fs.existsSync('../marker-{name}')) {{
    fs.writeFileSync('../marker-{name}', 'x');
    throw new Error('fails on the first run');
  }}
}});
"#
            ),
        );
    }
    write(
        tmp.path(),
        "app/check.js",
        "require('node:fs').appendFileSync('../check.log', 'ran\\n');\n",
    );
    write(
        tmp.path(),
        "app/post.js",
        "require('node:fs').appendFileSync('../post.log', 'ran\\n');\n",
    );
    write(
        tmp.path(),
        "app/aster.toml",
        "[targets.deps]\ncommand = \"true\"\n\n[targets.test]\ncommand = \"npm run test-ci\"\n",
    );
    tmp
}

#[test]
fn a_package_script_chain_is_recorded_per_step_and_each_flaky_step_is_retried() {
    let available = |program: &str| Command::new(program).arg("--version").output().is_ok();
    if !available("node") || !available("npm") {
        eprintln!("skipping: node or npm is not installed");
        return;
    }
    let tmp = chain_workspace();
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
    assert!(output.status.success(), "both retries pass: {stderr}");

    // Every step's tests are in one report; each flaky test has its failing
    // first attempt and its passing retry.
    let records = read_records(&results);
    assert_eq!(target_status(&records, "//app:test"), TargetStatus::Passed);
    let tests = tests_of(&records);
    for expected in [
        ("stable a", TestOutcome::Pass, 1),
        ("flaky a", TestOutcome::Error, 1),
        ("flaky a", TestOutcome::Pass, 2),
        ("stable b", TestOutcome::Pass, 1),
        ("flaky b", TestOutcome::Error, 1),
        ("flaky b", TestOutcome::Pass, 2),
    ] {
        assert!(
            tests.contains(&(expected.0.into(), expected.1, expected.2)),
            "{expected:?} in {tests:?}"
        );
    }
    assert_eq!(tests.len(), 6, "{tests:?}");
    // Aster plans each `node --test` step from its argv: complete.
    assert_eq!(
        confidence_of(&records, "//app:test"),
        Some(Confidence::Complete)
    );

    // Only the failing test of a step was rerun, and the steps that are not
    // test runners ran once each, after the first step's retry.
    for log in ["stable-a.log", "stable-b.log", "check.log", "post.log"] {
        let text = fs::read_to_string(tmp.path().join(log)).unwrap();
        assert_eq!(text.lines().count(), 1, "{log}: {text}");
    }
}

#[test]
fn a_failing_step_of_a_chain_stops_it_and_leaves_the_results_partial() {
    let available = |program: &str| Command::new(program).arg("--version").output().is_ok();
    if !available("node") || !available("npm") {
        eprintln!("skipping: node or npm is not installed");
        return;
    }
    // Without --retry-failed the first failure stops the chain, as `&&` does.
    let tmp = chain_workspace();
    let results = tmp.path().join("out/results.ndjson");
    let output = aster(
        tmp.path(),
        &[
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    assert!(!output.status.success());
    let records = read_records(&results);
    assert_eq!(target_status(&records, "//app:test"), TargetStatus::Failed);
    assert!(tests_of(&records).contains(&("flaky a".into(), TestOutcome::Error, 1)));
    assert!(!tmp.path().join("check.log").exists());
    assert_ne!(
        confidence_of(&records, "//app:test"),
        Some(Confidence::Complete),
        "steps that never ran leave the results partial"
    );
}

/// A chain `node --test a && node --test b && node post.js`: step 2 holds a
/// test that always fails (quarantined in the first scenario), optionally with
/// a second, real failure.
fn quarantine_chain_workspace(also_real: bool) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(
        tmp.path(),
        "app/package.json",
        r#"{"name":"app","scripts":{"test-ci":"node --test a.test.js && node --test b.test.js && node post.js"}}"#,
    );
    write(
        tmp.path(),
        "app/a.test.js",
        "require('node:test')('stable a', () => {});\n",
    );
    let real = if also_real {
        "test('real failure b', () => { throw new Error('a real failure'); });\n"
    } else {
        ""
    };
    write(
        tmp.path(),
        "app/b.test.js",
        &format!(
            "const test = require('node:test');\n\
             test('known flaky b', () => {{ throw new Error('flaky on CI'); }});\n\
             test('stable b', () => {{}});\n{real}"
        ),
    );
    write(
        tmp.path(),
        "app/post.js",
        "require('node:fs').writeFileSync('../post.ran', 'x');\n",
    );
    write(
        tmp.path(),
        "app/aster.toml",
        "[targets.deps]\ncommand = \"true\"\n\n[targets.test]\ncommand = \"npm run test-ci\"\n",
    );
    write(
        tmp.path(),
        "aster-quarantine.toml",
        "[[test]]\nname = \"known flaky b\"\ntarget = \"//app\"\nreason = \"flaky on CI\"\nexpires = 2999-01-01\n",
    );
    tmp
}

#[test]
fn a_quarantined_failure_in_a_chain_step_is_excused_and_the_chain_continues() {
    let available = |program: &str| Command::new(program).arg("--version").output().is_ok();
    if !available("node") || !available("npm") {
        eprintln!("skipping: node or npm is not installed");
        return;
    }
    // Only the quarantined test fails in step 2: the target passes and the
    // steps after it run.
    let tmp = quarantine_chain_workspace(false);
    let results = tmp.path().join("out/results.ndjson");
    let output = aster(
        tmp.path(),
        &[
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("(quarantined: 1)"), "{stderr}");
    assert!(tmp.path().join("post.ran").exists(), "the chain continued");
    let records = read_records(&results);
    assert_eq!(target_status(&records, "//app:test"), TargetStatus::Passed);
    let quarantined: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            Record::Test(t) if t.quarantined => Some(t.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(quarantined, ["known flaky b"]);
    assert!(tests_of(&records).contains(&("stable a".into(), TestOutcome::Pass, 1)));

    // Another failure in the same step is not quarantined: the target fails
    // and the chain stops.
    let tmp = quarantine_chain_workspace(true);
    let output = aster(
        tmp.path(),
        &["--no-cache", "--test-results=1", "test", "//app"],
    );
    assert!(!output.status.success());
    assert!(!tmp.path().join("post.ran").exists());
}

#[test]
fn a_declared_report_on_a_recognised_runner_is_still_retried() {
    let available = |program: &str| Command::new(program).arg("--version").output().is_ok();
    if !available("node") {
        eprintln!("skipping: node is not installed");
        return;
    }
    // Retrying needs the report to name each test's file; older Node
    // versions' JUnit reporter does not.
    let probe = TempDir::new().unwrap();
    write(
        probe.path(),
        "p.test.js",
        "require('node:test')('p', () => {});\n",
    );
    let probed = Command::new("node")
        .args([
            "--test",
            "--test-reporter=junit",
            "--test-reporter-destination=junit.xml",
            "p.test.js",
        ])
        .current_dir(probe.path())
        .output();
    let names_files = probed.is_ok()
        && fs::read_to_string(probe.path().join("junit.xml"))
            .is_ok_and(|xml| xml.contains(" file="));
    if !names_files {
        eprintln!("skipping: this Node's JUnit reporter does not name test files");
        return;
    }
    // The command picks its own reporter and writes JUnit to a file the
    // target declares; the runner is `node --test`, so the rerun is built
    // from it and each attempt is read from the declared file.
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(tmp.path(), "app/package.json", r#"{"name":"app"}"#);
    write(
        tmp.path(),
        "app/a.test.js",
        "const test = require('node:test');\nconst fs = require('node:fs');\n\
         test('stable', () => { fs.appendFileSync('../stable.log', 'ran\\n'); });\n\
         test('flaky', () => {\n  if (!fs.existsSync('../marker')) {\n    fs.writeFileSync('../marker', 'x');\n    throw new Error('first run');\n  }\n});\n",
    );
    write(
        tmp.path(),
        "app/aster.toml",
        "[targets.deps]\ncommand = \"true\"\n\n[targets.test]\n\
         command = \"node --test --test-reporter=junit --test-reporter-destination=junit.xml a.test.js\"\n\n\
         [targets.test.report]\nformat = \"junit\"\nfiles = [\"junit.xml\"]\n",
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
    let tests = tests_of(&read_records(&results));
    assert!(
        tests.contains(&("flaky".into(), TestOutcome::Pass, 2)),
        "{tests:?}"
    );
    let stable = fs::read_to_string(tmp.path().join("stable.log")).unwrap();
    assert_eq!(
        stable.lines().count(),
        1,
        "only the failing test reran: {stable}"
    );
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

fn aster_in_github(root: &Path, summary: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(root)
        .env_remove("ASTER_TEST_RESULTS")
        .env_remove("ASTER_RETRY_FAILED")
        .env_remove("ASTER_SRV_URL")
        .env_remove("ASTER_SRV_TOKEN")
        .env_remove("ASTER_GITHUB_SUMMARY")
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", summary)
        .env("RUNNER_TEMP", root.join("runner-temp"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn a_github_run_summarizes_and_annotates_its_failures() {
    let tmp = workspace();
    let results = tmp.path().join("out/results.ndjson");
    let step = tmp.path().join("step-summary.md");
    let output = aster_in_github(
        tmp.path(),
        &step,
        &[
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("::error title=failed%3A divides()::"),
        "{stderr}"
    );
    let summary = fs::read_to_string(&step).unwrap();
    assert!(summary.contains("## Aster test results"), "{summary}");
    assert!(summary.contains("**1 failing test**"), "{summary}");
    assert!(summary.contains("divides()"), "{summary}");

    // ASTER_GITHUB_SUMMARY=0 turns it off.
    fs::remove_file(&step).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &step)
        .env("ASTER_GITHUB_SUMMARY", "0")
        .env_remove("ASTER_SRV_URL")
        .args([
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!step.exists());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("::error"));
}

#[test]
fn results_summary_prints_markdown_or_github_commands() {
    let tmp = workspace();
    let results = tmp.path().join("out/results.ndjson");
    let run = aster(
        tmp.path(),
        &[
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//app",
        ],
    );
    assert!(!run.status.success());
    let results = results.display().to_string();

    let output = aster(tmp.path(), &["results", "summary", &results]);
    assert!(output.status.success());
    let markdown = String::from_utf8_lossy(&output.stdout);
    assert!(markdown.contains("| failed | divides() |"), "{markdown}");

    let step = tmp.path().join("step.md");
    let output = aster_in_github(
        tmp.path(),
        &step,
        &["results", "summary", &results, "--github"],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("::error "), "{stdout}");
    assert!(stdout.contains("title=failed%3A divides()"), "{stdout}");
    assert!(fs::read_to_string(&step).unwrap().contains("divides()"));
}

#[test]
fn results_failures_exits_nonzero_when_it_cannot_ask() {
    let tmp = workspace();
    let run = |url: Option<&str>, token: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aster"));
        command
            .current_dir(tmp.path())
            .env_remove("ASTER_SRV_URL")
            .env_remove("ASTER_SRV_TOKEN")
            .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
            .args(["results", "failures", "--github-run", "123"]);
        if let Some(url) = url {
            command.args(["--url", url]);
        }
        if let Some(token) = token {
            command.env("ASTER_SRV_TOKEN", token);
        }
        command.output().unwrap()
    };
    let unreachable = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    for (url, token, message) in [
        (None, None, "give --url or set ASTER_SRV_URL"),
        (Some("http://127.0.0.1:1"), None, "no credentials"),
        (Some(unreachable.as_str()), Some("t"), "cannot reach"),
    ] {
        let output = run(url, token);
        assert_eq!(output.status.code(), Some(1), "{message}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{stderr}");
    }
}

/// Answers one request with `body` and hands back the request line.
fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        request_line
    });
    (url, handle)
}

/// Two shard projects that each run a partition of `app`'s suite, and a
/// `mix` on PATH that writes what it was asked to run to
/// `app/ran-<partition>.txt`.
#[cfg(unix)]
fn partitioned_workspace() -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(tmp.path(), "app/mix.exs", "[app: :app]\n");
    for file in ["a", "b", "c", "d"] {
        write(tmp.path(), &format!("app/test/{file}_test.exs"), "");
    }
    for k in [1, 2] {
        write(
            tmp.path(),
            &format!("shard-{k}/package.json"),
            &format!(r#"{{"name":"shard-{k}","private":true}}"#),
        );
        write(
            tmp.path(),
            &format!("shard-{k}/aster.toml"),
            &format!(
                "[targets.test-ci]\ncommand = \"bash -c 'cd ../app && MIX_TEST_PARTITION={k} exec mix test --partitions 2'\"\n"
            ),
        );
    }
    write(
        tmp.path(),
        "bin/mix",
        "#!/bin/sh\necho \"$*\" > \"ran-$MIX_TEST_PARTITION.txt\"\n",
    );
    let mix = tmp.path().join("bin/mix");
    fs::set_permissions(&mix, fs::Permissions::from_mode(0o755)).unwrap();
    tmp
}

#[cfg(unix)]
fn aster_with_mix(root: &Path, args: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(root)
        .env("PATH", path)
        .env_remove("ASTER_SRV_URL")
        .env_remove("ASTER_BALANCE_PARTITIONS")
        .env("ASTER_SRV_TOKEN", "t")
        .args(args)
        .output()
        .unwrap()
}

#[test]
#[cfg(unix)]
fn shards_split_by_the_committed_durations_file_without_a_server() {
    let ws = partitioned_workspace();
    let root = ws.path();
    // Without the file, Mix deals the files out.
    let output = aster_with_mix(root, &["run", "//shard-1:test-ci", "--no-cache"]);
    let log = String::from_utf8_lossy(&output.stderr).to_string()
        + &String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    let ran = |k: u32| fs::read_to_string(root.join(format!("app/ran-{k}.txt"))).unwrap();
    assert_eq!(ran(1), "test --partitions 2\n");
    assert!(
        log.contains("split by Mix (no durations for app in aster-durations.json)"),
        "{log}"
    );

    // `aster results durations` writes the file from the server.
    let (url, server) = serve_once(
        r#"{"files":[{"file":"test/a_test.exs","p50_ms":100,"runs":3},{"file":"test/b_test.exs","p50_ms":10,"runs":3},{"file":"test/c_test.exs","p50_ms":10,"runs":3},{"file":"test/d_test.exs","p50_ms":80,"runs":3}]}"#,
    );
    let output = aster_with_mix(root, &["results", "durations", "--url", &url]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = server.join().unwrap();
    assert!(
        request.contains("target=%2F%2Fshard-1%3Atest-ci&target=%2F%2Fshard-2%3Atest-ci"),
        "{request}"
    );
    let file: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("aster-durations.json")).unwrap())
            .unwrap();
    assert_eq!(file["suites"]["app"]["files"]["test/a_test.exs"], 100);

    // Each shard now runs its share by name, with no server configured.
    let run = |shard: &str| {
        let output = aster_with_mix(root, &["run", &format!("//{shard}:test-ci"), "--no-cache"]);
        let log = String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{log}");
        log
    };
    let first = run("shard-1");
    assert!(
        first.contains("balanced by duration: 1 of 4 files"),
        "{first}"
    );
    assert_eq!(ran(1), "test test/a_test.exs\n");
    run("shard-2");
    assert_eq!(
        ran(2),
        "test test/b_test.exs test/c_test.exs test/d_test.exs\n"
    );
}

// Quarantine, end to end with a real `go test`.

/// A Go module with a test that always fails (`TestQuarantined`), one that
/// passes, and, when `also_failing`, a second failing test. `after` depends
/// on `test` and records that it ran.
fn quarantine_go_workspace(also_failing: bool, quarantine_file: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(
        tmp.path(),
        "app/go.mod",
        "module example.com/app\n\ngo 1.21\n",
    );
    let other = if also_failing {
        "\nfunc TestReal(t *testing.T) { t.Fatal(\"a real failure\") }\n"
    } else {
        ""
    };
    write(
        tmp.path(),
        "app/app_test.go",
        &format!(
            "package app\n\nimport \"testing\"\n\n\
             func TestFine(t *testing.T) {{}}\n\n\
             func TestQuarantined(t *testing.T) {{ t.Fatal(\"flaky on CI\") }}\n{other}"
        ),
    );
    write(
        tmp.path(),
        "app/aster.toml",
        "[targets.deps]\ncommand = \"true\"\n\n[targets.test]\ncommand = \"go test ./...\"\n\n\
         [targets.after]\ncommand = \"touch ../after.ran\"\ndepends_on = [\"//self:test\"]\n",
    );
    if !quarantine_file.is_empty() {
        write(tmp.path(), "aster-quarantine.toml", quarantine_file);
    }
    tmp
}

const QUARANTINE_FILE: &str = r#"
[[test]]
name = "TestQuarantined"
target = "//app"
reason = "flaky on CI, see #1"
expires = 2999-01-01
"#;

#[test]
fn a_target_whose_only_failures_are_quarantined_passes_and_says_so() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = quarantine_go_workspace(false, QUARANTINE_FILE);
    let results = tmp.path().join("out/results.ndjson");
    let step = tmp.path().join("step-summary.md");
    let output = aster_in_github(
        tmp.path(),
        &step,
        &[
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "run",
            "//app:after",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stderr}\n{stdout}");
    assert!(
        stderr.contains("[aster] Quarantine: 1 from aster-quarantine.toml"),
        "{stderr}"
    );
    assert!(stderr.contains("(quarantined: 1)"), "{stderr}");
    // Go names a test's package, not its file.
    assert!(
        stderr.contains("::warning::TestQuarantined failed but is quarantined"),
        "{stderr}"
    );
    // The dependent target ran.
    assert!(tmp.path().join("after.ran").exists());

    let records = read_records(&results);
    assert_eq!(target_status(&records, "//app:test"), TargetStatus::Passed);
    let quarantined: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            Record::Test(t) if t.quarantined => Some(t.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(quarantined, ["TestQuarantined"]);
    let summary = fs::read_to_string(&step).unwrap();
    assert!(summary.contains("1 quarantined"), "{summary}");
    assert!(
        summary.contains("| quarantined | TestQuarantined"),
        "{summary}"
    );
    // The run already warned; the summary adds no error for it.
    assert!(!stderr.contains("::error"), "{stderr}");
}

#[test]
fn any_other_failure_still_fails_the_target() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = quarantine_go_workspace(true, QUARANTINE_FILE);
    let output = aster(
        tmp.path(),
        &["--no-cache", "--test-results=out/r.ndjson", "test", "//app"],
    );
    assert!(!output.status.success());
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all.contains("1 of 2 failing tests are quarantined; the others still fail"),
        "{all}"
    );
}

#[test]
fn quarantine_can_be_turned_off_and_needs_captured_results() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = quarantine_go_workspace(false, QUARANTINE_FILE);
    let off = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_QUARANTINE", "off")
        .env_remove("ASTER_SRV_URL")
        .env("RUNNER_TEMP", tmp.path().join("runner-temp"))
        .args(["--no-cache", "--test-results=out/r.ndjson", "test", "//app"])
        .output()
        .unwrap();
    assert!(!off.status.success());
    assert!(String::from_utf8_lossy(&off.stderr).contains("Quarantine: off"));
    // Without --test-results or --retry-failed nothing is captured, so
    // nothing is let through.
    let plain = aster(tmp.path(), &["--no-cache", "test", "//app"]);
    assert!(!plain.status.success());
}

#[test]
fn a_bad_quarantine_file_is_an_error() {
    if !go_available() {
        eprintln!("skipping: go is not installed");
        return;
    }
    let tmp = quarantine_go_workspace(false, "[[test]]\nname = \"TestQuarantined\"\n");
    let output = aster(
        tmp.path(),
        &["--no-cache", "--test-results=out/r.ndjson", "test", "//app"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("aster-quarantine.toml"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// Declared reports: a target whose runner hides behind a script.

/// A Go module whose `test` target runs `tools/run-tests.sh`, as
/// firstlanding's Rust and Go lanes wrap `cargo nextest` and `go test` in
/// scripts. The script writes `REPORT` where `write_to` says and fails.
fn wrapped_workspace(write_to: &str, report: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(
        tmp.path(),
        "svc/go.mod",
        "module example.com/svc\n\ngo 1.21\n",
    );
    write(tmp.path(), "svc/svc_test.go", "package svc\n");
    fs::write(tmp.path().join("report.xml"), REPORT).unwrap();
    write(
        tmp.path(),
        "svc/tools/run-tests.sh",
        &format!("#!/bin/sh\nmkdir -p \"$(dirname \"{write_to}\")\"\ncp ../report.xml \"{write_to}\"\nexit 1\n"),
    );
    write(
        tmp.path(),
        "svc/aster.toml",
        &format!(
            "[targets.deps]\ncommand = \"true\"\n\n[targets.test]\ncommand = \"sh tools/run-tests.sh\"\n{report}"
        ),
    );
    tmp
}

fn run_wrapped(tmp: &TempDir) -> (Output, Vec<Record>) {
    let results = tmp.path().join("out/results.ndjson");
    let output = aster(
        tmp.path(),
        &[
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "test",
            "//svc",
        ],
    );
    assert!(!output.status.success(), "the failing test fails the run");
    (output, read_records(&results))
}

fn confidence_of(records: &[Record], address: &str) -> Option<Confidence> {
    records.iter().find_map(|r| match r {
        Record::Target(t) if t.address == address => t.confidence,
        _ => None,
    })
}

#[test]
fn a_wrapped_runner_records_nothing_until_its_report_is_declared() {
    // Before: the script hides the runner, so its tests are not recorded,
    // the "(0 tests)" firstlanding's Rust lane uploaded.
    let tmp = wrapped_workspace("$ASTER_TEST_REPORT_DIR/junit.xml", "");
    let (_, records) = run_wrapped(&tmp);
    assert!(tests_of(&records).is_empty());

    // Declared, the script writes into the directory Aster names in
    // ASTER_TEST_REPORT_DIR and both tests are recorded, complete.
    let tmp = wrapped_workspace(
        "$ASTER_TEST_REPORT_DIR/junit.xml",
        "\n[targets.test.report]\nformat = \"junit\"\n",
    );
    let (output, records) = run_wrapped(&tmp);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Test results (2 tests) written to"),
        "{stderr}"
    );
    assert_eq!(
        tests_of(&records),
        vec![
            ("adds()".to_string(), TestOutcome::Pass, 1),
            ("divides()".to_string(), TestOutcome::Fail, 1),
        ]
    );
    assert_eq!(
        confidence_of(&records, "//svc:test"),
        Some(Confidence::Complete)
    );
    // The report directory is removed after it is read.
    let reports = tmp.path().join("runner-temp/aster-reports");
    assert!(fs::read_dir(&reports).map_or(true, |mut d| d.next().is_none()));
}

#[test]
fn declared_report_files_in_the_project_are_read_only_when_written_by_this_run() {
    // nextest's JUnit profile writes into the project's target directory.
    let tmp = wrapped_workspace(
        "target/nextest/ci/junit.xml",
        "\n[targets.test.report]\nformat = \"junit\"\nfiles = [\"target/nextest/*/junit.xml\"]\n",
    );
    // A report left by an earlier run is not this run's.
    write(
        tmp.path(),
        "svc/target/nextest/old/junit.xml",
        r#"<testsuite name="Old"><testcase classname="Old" name="stale"/></testsuite>"#,
    );
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(tmp.path().join("svc/target/nextest/old/junit.xml"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let (_, records) = run_wrapped(&tmp);
    let names: Vec<String> = tests_of(&records).into_iter().map(|t| t.0).collect();
    assert_eq!(names, vec!["adds()", "divides()"]);
}

#[test]
fn quarantine_never_lets_a_declared_report_pass() {
    // Every failing test is quarantined and the script exits 1, the code a
    // JUnit runner uses for failed tests. But the script could have failed
    // in any of its steps, so the target still fails.
    let tmp = wrapped_workspace(
        "$ASTER_TEST_REPORT_DIR/junit.xml",
        "\n[targets.test.report]\nformat = \"junit\"\n",
    );
    write(
        tmp.path(),
        "aster-quarantine.toml",
        "[[test]]\nname = \"divides()\"\ntarget = \"//svc\"\nreason = \"flaky\"\nexpires = 2999-01-01\n",
    );
    let (output, records) = run_wrapped(&tmp);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot tell this runner's test failures from other errors"),
        "{stderr}"
    );
    assert_eq!(target_status(&records, "//svc:test"), TargetStatus::Failed);
    let capture = records.iter().find_map(|r| match r {
        Record::Target(t) if t.address == "//svc:test" => t.capture.clone(),
        _ => None,
    });
    assert_eq!(capture.as_deref(), Some("declared-junit"));
}

#[test]
fn a_bad_declared_report_is_a_configuration_error() {
    for report in [
        "\n[targets.test.report]\nformat = \"tap\"\n",
        "\n[targets.test.report]\nformat = \"junit\"\nfiles = [\"../target/junit.xml\"]\n",
        "\n[targets.test.report]\nformat = \"junit\"\nfiles = [\"./target/junit.xml\"]\n",
        "\n[targets.test.report]\nformat = \"junit\"\nfiles = [\"[\"]\n",
        "\n[targets.test.report]\nformat = \"junit\"\npath = \"x\"\n",
    ] {
        let tmp = wrapped_workspace("$ASTER_TEST_REPORT_DIR/junit.xml", report);
        let output = aster(tmp.path(), &["test", "//svc"]);
        assert!(!output.status.success(), "{report}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("aster.toml"), "{report}: {stderr}");
    }
}

// Vitest commands that carry their own JUnit report or shell expansions.

/// A stand-in for the `vitest` binary that behaves like Vitest 4 for what
/// matters here: it writes JUnit to `--outputFile.junit` (relative to the
/// working directory) and fails because `flaky one` fails. A second
/// `--outputFile.junit` becomes an array in Vitest's option parser and its
/// JUnit reporter throws in `onInit`, so the stand-in crashes with no report.
const FAKE_VITEST: &str = r#"#!/bin/sh
out=""; n=0; junit=0
for arg in "$@"; do
  case "$arg" in
    --outputFile.junit=*) out="${arg#--outputFile.junit=}"; n=$((n + 1)) ;;
    --reporter=junit) junit=1 ;;
  esac
done
echo "ran: $*" >> ../vitest.calls
if [ "$n" -gt 1 ]; then
  echo "TypeError: input.replace is not a function" >&2
  exit 1
fi
if [ "$junit" = 1 ] && [ -n "$out" ]; then
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'XML'
<testsuites><testsuite name="flaky.e2e.test.ts" tests="2" failures="1"><testcase classname="flaky.e2e.test.ts" name="flaky one" time="0.003"><failure message="expected 1 to be 2">at flaky.e2e.test.ts:2</failure></testcase><testcase classname="flaky.e2e.test.ts" name="fine one" time="0.001"/></testsuite></testsuites>
XML
fi
exit 1
"#;

fn vitest_workspace(command: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    write(tmp.path(), "web/package.json", r#"{"name":"web"}"#);
    write(tmp.path(), "web/bin/vitest", FAKE_VITEST);
    use std::os::unix::fs::PermissionsExt;
    let bin = tmp.path().join("web/bin/vitest");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    write(
        tmp.path(),
        "web/aster.toml",
        &format!(
            "[targets.deps]\ncommand = \"true\"\n\n[targets.e2e]\ncommand = {}\n",
            serde_json::to_string(command).unwrap()
        ),
    );
    write(
        tmp.path(),
        "aster-quarantine.toml",
        "[[test]]\nname = \"flaky one\"\ntarget = \"//web\"\nreason = \"flaky on CI\"\nexpires = 2999-01-01\n",
    );
    tmp
}

fn run_e2e(tmp: &TempDir, envs: &[(&str, &str)]) -> (Output, Vec<Record>) {
    let results = tmp.path().join("out/results.ndjson");
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env_remove("ASTER_TEST_RESULTS")
        .env_remove("ASTER_RETRY_FAILED")
        .env_remove("ASTER_SRV_URL")
        .env_remove("ASTER_SRV_TOKEN")
        .env_remove("GITHUB_ACTIONS")
        .env("RUNNER_TEMP", tmp.path().join("runner-temp"))
        .envs(envs.iter().copied())
        .args([
            "--no-cache",
            &format!("--test-results={}", results.display()),
            "run",
            "//web:e2e",
        ])
        .output()
        .unwrap();
    let records = fs::read_to_string(&results)
        .map(|text| parse_ndjson(&text).unwrap())
        .unwrap_or_default();
    (output, records)
}

#[test]
fn a_vitest_command_with_its_own_junit_report_and_expansions_is_captured_and_quarantined() {
    // firstlanding's browser e2e command: a shell one-liner whose file filter
    // and report path are `${VAR:-default}` expansions, with a JUnit reporter
    // of its own. Captured 0 tests before, so quarantine could not apply.
    let tmp = vitest_workspace(
        "bash -c 'bin/vitest run --root . ${E2E_FILE:-e2e} --reporter=default --reporter=junit \
         --outputFile.junit=\"${E2E_RESULTS:-out/e2e.xml}\"'",
    );
    let (output, records) = run_e2e(&tmp, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("(quarantined: 1)"), "{stderr}");
    assert!(
        !stderr.contains("no test results were captured"),
        "{stderr}"
    );
    assert_eq!(tests_of(&records).len(), 2, "{stderr}");
    // The command ran once, as written: no second reporter or output file.
    let calls = fs::read_to_string(tmp.path().join("vitest.calls")).unwrap();
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(calls.contains("--root . e2e --reporter=default"), "{calls}");

    // The variable can name the file; Aster reads whichever it expands to.
    let (output, records) = run_e2e(&tmp, &[("E2E_RESULTS", "out/other.xml")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(tests_of(&records).len(), 2);
    assert!(tmp.path().join("web/out/other.xml").exists());
}

#[test]
fn a_report_path_the_one_liner_sets_itself_is_not_guessed_and_the_run_says_so() {
    let tmp = vitest_workspace(
        "bash -c 'OUT=out/set.xml; bin/vitest run --root . e2e --reporter=default --reporter=junit \
         --outputFile.junit=$OUT'",
    );
    let (output, records) = run_e2e(&tmp, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Nothing is added to the command, so the stand-in still gets a single
    // `--outputFile.junit` and writes its report, but Aster cannot know where.
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("[aster] Warning: no test results were captured for //web:e2e"),
        "{stderr}"
    );
    assert!(tests_of(&records).is_empty());
}
