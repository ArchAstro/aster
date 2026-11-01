//! End-to-end tests for test commands that hide their runners behind a
//! package script or a script file: `aster affected --related` narrows what
//! it can read, and `aster check` reports what it cannot.

use serde_json::Value;
use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Workspace {
    tmp: TempDir,
}

impl Workspace {
    /// A committed git workspace holding `files`.
    fn new(files: &[(&str, &str)]) -> Self {
        let workspace = Self {
            tmp: TempDir::new().unwrap(),
        };
        workspace.git(&["init", "-q", "-b", "main"]);
        workspace.git(&["config", "user.email", "test@test.com"]);
        workspace.git(&["config", "user.name", "Test"]);
        workspace.write("aster.toml", "");
        for (path, content) in files {
            workspace.write(path, content);
        }
        workspace.git(&["add", "-A"]);
        workspace.git(&["commit", "-q", "-m", "initial"]);
        workspace
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.tmp.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed: {output:?}");
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.tmp.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn aster(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(self.tmp.path())
            .env("ASTER_CACHE_DIR", self.tmp.path().join(".cache"))
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.aster(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON ({error}): {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    /// The `(commands, selection notes)` of each target in the dry-run plan
    /// of `aster affected <target> --related`, by address.
    fn plan(&self, target: &str) -> Vec<(String, Vec<String>, Vec<String>)> {
        let json = self.json(&[
            "--json",
            "affected",
            target,
            "--base=HEAD",
            "--related",
            "--dry-run",
        ]);
        let strings = |entry: &Value, key: &str| -> Vec<String> {
            entry[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        json["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["address"].as_str().unwrap().ends_with(target))
            .map(|entry| {
                (
                    entry["address"].as_str().unwrap().to_string(),
                    strings(entry, "commands"),
                    strings(entry, "selection"),
                )
            })
            .collect()
    }

    /// The targets `aster check --rule unnarrowable-test-command --entry
    /// <entry>` reports, with their messages.
    fn unnarrowable(&self, entry: &str) -> Vec<(String, String)> {
        let json = self.json(&[
            "--json",
            "check",
            "--rule",
            "unnarrowable-test-command",
            "--entry",
            entry,
        ]);
        json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["target"].as_str().unwrap().to_string(),
                    f["message"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

/// A TypeScript project with one source file and the test that imports it.
fn project(name: &str, scripts: &str, target_command: &str) -> Vec<(String, String)> {
    vec![
        (
            format!("{name}/package.json"),
            format!(r#"{{"name":"{name}","scripts":{{{scripts}}}}}"#),
        ),
        (
            format!("{name}/aster.toml"),
            format!("[targets.test-ci]\ncommand = {target_command:?}\n"),
        ),
        (
            format!("{name}/src/lib.ts"),
            "export const one = () => 1;\n".to_string(),
        ),
        (
            format!("{name}/src/lib.test.ts"),
            "import { one } from \"./lib\";\nit(\"one\", () => { expect(one()).toBe(1); });\n"
                .to_string(),
        ),
        (
            format!("{name}/src/other.test.ts"),
            "it(\"other\", () => { expect(2).toBe(2); });\n".to_string(),
        ),
    ]
}

#[test]
fn a_package_script_chain_narrows_and_what_aster_cannot_read_is_reported() {
    // Setup: five projects whose `test-ci` target hides its runner in
    // different ways.
    let mut files = Vec::new();
    // Narrowable: a chain through a nested script, ending in a type check.
    files.extend(project(
        "chain",
        r#""test-ci":"pnpm unit && tsc --noEmit","unit":"vitest run""#,
        "pnpm test-ci",
    ));
    // Not narrowable: Aster does not read a script file.
    files.extend(project("file", "", "./test-ci.sh"));
    // Not narrowable: yarn has no `exec` that finds the package binaries.
    files.extend(project(
        "yarn",
        r#""test-ci":"vitest run && tsc""#,
        "yarn test-ci",
    ));
    // Not narrowable: a package script with no test runner.
    files.extend(project("lint", r#""test-ci":"eslint .""#, "pnpm test-ci"));
    // Nothing to narrow: a target that only groups others.
    files.extend(project("group", "", "true"));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let ws = Workspace::new(&files);

    // Action: change every project's source.
    for name in ["chain", "file", "yarn", "lint", "group"] {
        ws.write(
            &format!("{name}/src/lib.ts"),
            "export const one = () => 1 + 0;\n",
        );
    }

    // Outcome 1: the chain narrows to the test that reaches the change; the
    // type check stays.
    let plan = ws.plan("test-ci");
    let chain = plan
        .iter()
        .find(|(address, ..)| address == "//chain:test-ci")
        .expect("//chain:test-ci runs");
    assert_eq!(
        chain.1,
        [
            "pnpm exec vitest run src/lib.test.ts --coverage.enabled=false --passWithNoTests",
            "pnpm exec tsc --noEmit"
        ]
    );

    // Outcome 2: the planner runs the others as written, and says why.
    let mut not_narrowed: Vec<&str> = plan
        .iter()
        .filter(|(_, _, notes)| {
            notes
                .iter()
                .any(|n| n.contains("not a test command Aster can narrow"))
        })
        .map(|(address, ..)| address.as_str())
        .collect();
    not_narrowed.sort();
    assert_eq!(
        not_narrowed,
        ["//file:test-ci", "//lint:test-ci", "//yarn:test-ci"]
    );

    // Outcome 3: `aster check` reports exactly those targets, with a reason
    // that names the cause, so the two cannot drift.
    let mut findings = ws.unnarrowable("test-ci");
    findings.sort();
    let reported: Vec<&str> = findings.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(reported, not_narrowed);
    let message = |target: &str| {
        findings
            .iter()
            .find(|(t, _)| t == target)
            .map(|(_, m)| m.as_str())
            .unwrap()
    };
    assert!(
        message("//file:test-ci").contains("script file `./test-ci.sh`, which Aster does not read"),
        "{findings:?}"
    );
    assert!(
        message("//yarn:test-ci")
            .contains("package script `test-ci` contains `vitest run`, which Aster cannot narrow"),
        "{findings:?}"
    );
    assert!(
        message("//lint:test-ci")
            .contains("package script `test-ci` (`eslint .`) contains no test runner"),
        "{findings:?}"
    );
}

#[test]
fn a_chain_with_no_selected_test_is_skipped_with_its_other_commands() {
    let files: Vec<(String, String)> = project(
        "web",
        r#""test-ci":"node gen.mjs --check && vitest run src/lib.test.ts && tsc --noEmit""#,
        "pnpm test-ci",
    );
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let ws = Workspace::new(&files);
    // Only a test the script does not list reaches this change: the lone
    // runner is dropped, and the commands around it do not run alone.
    ws.write(
        "web/src/other.test.ts",
        "it(\"other\", () => { expect(3).toBe(3); });\n",
    );
    let plan = ws.plan("test-ci");
    assert!(plan.is_empty(), "{plan:?}");
}

#[test]
fn a_coverage_gated_chain_runs_its_narrowed_tests_without_coverage() {
    // Setup: `test-ci` runs Vitest with coverage and a config that sets
    // thresholds for the whole suite.
    let files = project(
        "web",
        r#""test-ci":"vitest run --coverage --config vitest.review-coverage.config.ts && tsc --noEmit""#,
        "pnpm test-ci",
    );
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let ws = Workspace::new(&files);
    ws.write("web/src/lib.ts", "export const one = () => 1 + 0;\n");

    // Outcome: the subset runs with coverage switched off, so a threshold
    // written for the whole suite cannot fail it.
    let plan = ws.plan("test-ci");
    assert_eq!(
        plan[0].1,
        [
            "pnpm exec vitest run --config vitest.review-coverage.config.ts src/lib.test.ts --coverage.enabled=false --passWithNoTests",
            "pnpm exec tsc --noEmit"
        ]
    );
}

/// A stand-in for `vitest` that writes the JUnit report Aster asks for.
/// The first run fails `one`; a retry (`-t`) passes it. Appends each
/// invocation to the path in `ASTER_VITEST_LOG`.
const VITEST_STUB: &str = r#"#!/bin/sh
echo "$0 $*" >> "$ASTER_VITEST_LOG"
out=""
retry=0
prev=""
for arg in "$@"; do
  case "$arg" in
    --outputFile.junit=*) out="${arg#--outputFile.junit=}" ;;
    -t|--testNamePattern) retry=1 ;;
  esac
  case "$prev" in
    -t|--testNamePattern) retry=1 ;;
  esac
  prev="$arg"
done
if [ -z "$out" ]; then
  echo "vitest stub: no junit output file" >&2
  exit 1
fi
mkdir -p "$(dirname "$out")"
if [ "$retry" = 1 ]; then
  cat > "$out" <<'XML'
<testsuites><testsuite name="src/lib.test.ts"><testcase classname="src/lib.test.ts" name="one" time="0.001"/></testsuite></testsuites>
XML
  exit 0
fi
cat > "$out" <<'XML'
<testsuites><testsuite name="src/lib.test.ts"><testcase classname="src/lib.test.ts" name="one" time="0.001"><failure message="no">fail</failure></testcase></testsuite></testsuites>
XML
exit 1
"#;

fn ndjson(path: &std::path::Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn recorded_tests(records: &[Value], address: &str) -> Vec<(String, String, u64)> {
    let mut tests: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "test" && record["target"] == address)
        .map(|record| {
            (
                record["name"].as_str().unwrap().to_string(),
                record["outcome"].as_str().unwrap().to_string(),
                record["attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    tests.sort();
    tests
}

fn capture_of(records: &[Value], address: &str) -> Option<String> {
    records.iter().find_map(|record| {
        (record["type"] == "target" && record["address"] == address)
            .then(|| record["capture"].as_str().map(str::to_string))
            .flatten()
    })
}

#[test]
fn a_narrowed_chain_with_a_trailing_command_records_its_tests() {
    if Command::new("npm").arg("--version").output().is_err() {
        eprintln!("skipping: npm is not installed");
        return;
    }
    // The issue's script: Vitest, then a command that is not a test.
    // `npm test` is what a lockfile-less `npm install` project runs.
    let mut files = project("web", r#""test":"vitest run && echo done""#, "npm test");
    files.push((
        "web/node_modules/.bin/vitest".to_string(),
        VITEST_STUB.to_string(),
    ));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let ws = Workspace::new(&files);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = ws.tmp.path().join("web/node_modules/.bin/vitest");
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(bin, perms).unwrap();
    }
    ws.commit_all("make vitest executable");
    let log = ws.tmp.path().join("vitest.calls");
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(ws.tmp.path())
            .env("ASTER_CACHE_DIR", ws.tmp.path().join(".cache"))
            .env("ASTER_VITEST_LOG", &log)
            .env("RUNNER_TEMP", ws.tmp.path().join("runner-temp"))
            .env_remove("GITHUB_ACTIONS")
            .env_remove("ASTER_TEST_RESULTS")
            .env_remove("ASTER_RETRY_FAILED")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "aster {args:?} failed: {stderr}\n{stdout}"
        );
        (stdout, stderr)
    };

    let full_path = ws.tmp.path().join("full.ndjson");
    let _ = fs::remove_file(&log);
    let _ = run(&[
        "--no-cache",
        "--retry-failed=1",
        &format!("--test-results={}", full_path.display()),
        "affected",
        "test-ci",
        "--all",
    ]);
    let full = ndjson(&full_path);
    let address = "//web:test-ci";
    let full_tests = recorded_tests(&full, address);
    assert_eq!(
        capture_of(&full, address).as_deref(),
        Some("junit"),
        "full run did not capture: {full:?}"
    );
    assert_eq!(
        full_tests,
        [
            ("one".into(), "fail".into(), 1),
            ("one".into(), "pass".into(), 2)
        ],
        "full run tests: {full_tests:?}"
    );

    // The change reaches only src/lib.test.ts. The trailing `echo done` stays.
    ws.write("web/src/lib.ts", "export const one = () => 1 + 0;\n");
    let plan = ws.plan("test-ci");
    assert_eq!(
        plan[0].1,
        [
            "npm exec -c 'vitest run src/lib.test.ts --coverage.enabled=false --passWithNoTests'",
            "npm exec -c 'echo done'",
        ],
        "narrowed commands: {plan:?}"
    );

    let rel_path = ws.tmp.path().join("rel.ndjson");
    let _ = fs::remove_file(&log);
    let (_stdout, stderr) = run(&[
        "--no-cache",
        "--retry-failed=1",
        &format!("--test-results={}", rel_path.display()),
        "affected",
        "test-ci",
        "--base=HEAD",
        "--related",
    ]);
    let related = ndjson(&rel_path);
    assert_eq!(
        capture_of(&related, address).as_deref(),
        Some("junit"),
        "narrowed run did not capture: {related:?}\n{stderr}"
    );
    assert_eq!(
        recorded_tests(&related, address),
        full_tests,
        "narrowed tests differ from the full run\n{stderr}"
    );
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls
            .lines()
            .any(|line| line.contains(" -t ") || line.contains(" -t'")),
        "the failing test was not retried: {calls}\n{stderr}"
    );
    let stored =
        fs::read_to_string(ws.tmp.path().join(".aster/logs/latest.json")).unwrap_or_default();
    assert!(
        stored.contains("echo done") && stored.contains("done"),
        "the trailing command did not run: {stored}"
    );

    // The full command is a chain Aster can read, and the narrowed run
    // captured it, so check must not flag the target.
    let findings = ws.json(&["--json", "check", "--rule", "uncaptured-test-results"]);
    let flagged: Vec<_> = findings["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finding| finding["target"] == address)
        .collect();
    assert!(
        flagged.is_empty(),
        "check disagreed with the captured run: {flagged:?}"
    );
}
