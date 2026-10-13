//! `go test -json` (test2json) streams.
//!
//! The stream replaces `go test`'s console text, so besides reading results
//! this module renders the stream back to text for the target's log.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use super::{TestCase, TestOutcome};

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package", default)]
    package: Option<String>,
    #[serde(rename = "Test", default)]
    test: Option<String>,
    #[serde(rename = "Elapsed", default)]
    elapsed: Option<f64>,
    #[serde(rename = "Output", default)]
    output: Option<String>,
}

enum Line<'a> {
    Event(Event),
    /// Not a test2json event: text from a tool that wrote to the console
    /// directly (older Go versions print build errors this way).
    Text(&'a str),
}

fn lines(output: &str) -> impl Iterator<Item = Line<'_>> {
    output.lines().map(|line| {
        if line.starts_with('{') {
            if let Ok(event) = serde_json::from_str::<Event>(line) {
                return Line::Event(event);
            }
        }
        Line::Text(line)
    })
}

/// Lines test2json adds that `go test` without `-v` does not print.
fn is_progress(line: &str) -> bool {
    ["=== RUN", "=== PAUSE", "=== CONT", "=== NAME"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

pub(super) struct Parsed {
    pub cases: Vec<TestCase>,
    /// False when a test never reported a result or a package failed
    /// outside its tests (a build failure, a crash in `TestMain`, a
    /// timeout), so some tests may be missing.
    pub complete: bool,
}

pub(super) fn parse(output: &str) -> Parsed {
    struct State {
        package: String,
        test: String,
        output: String,
        outcome: Option<TestOutcome>,
        elapsed: Option<f64>,
    }
    let mut order: Vec<State> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut failed_packages: HashSet<String> = HashSet::new();
    let mut packages_with_failed_tests: HashSet<String> = HashSet::new();

    for line in lines(output) {
        let Line::Event(event) = line else { continue };
        let package = event.package.clone().unwrap_or_default();
        let Some(test) = event.test.clone() else {
            if matches!(event.action.as_str(), "fail" | "build-fail") {
                failed_packages.insert(package);
            }
            continue;
        };
        let at = *index
            .entry((package.clone(), test.clone()))
            .or_insert_with(|| {
                order.push(State {
                    package: package.clone(),
                    test,
                    output: String::new(),
                    outcome: None,
                    elapsed: None,
                });
                order.len() - 1
            });
        let state = &mut order[at];
        match event.action.as_str() {
            "output" => {
                if let Some(text) = event.output {
                    if !is_progress(&text) {
                        state.output.push_str(&text);
                    }
                }
            }
            "pass" | "fail" | "skip" => {
                state.outcome = Some(match event.action.as_str() {
                    "pass" => TestOutcome::Pass,
                    "fail" => TestOutcome::Fail,
                    _ => TestOutcome::Skip,
                });
                state.elapsed = event.elapsed;
                if event.action == "fail" {
                    packages_with_failed_tests.insert(package);
                }
            }
            _ => {}
        }
    }

    let mut complete = failed_packages
        .iter()
        .all(|package| packages_with_failed_tests.contains(package));
    let cases = order
        .into_iter()
        .map(|state| {
            let outcome = state.outcome.unwrap_or_else(|| {
                complete = false;
                TestOutcome::Error
            });
            let message = outcome.is_failure().then(|| {
                let text = state.output.trim();
                if state.outcome.is_none() {
                    format!("{text}\n\nThe test reported no result before the test binary exited.")
                        .trim()
                        .to_string()
                } else {
                    text.to_string()
                }
            });
            TestCase {
                file: None,
                suite: Some(state.package),
                name: state.test,
                outcome,
                attempt: 1,
                duration_ms: state
                    .elapsed
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .map(|seconds| (seconds * 1000.0).round() as u64),
                message: message.filter(|m| !m.is_empty()),
            }
        })
        .collect();
    Parsed { cases, complete }
}

/// The text `go test` would have printed.
///
/// With `verbose` (the command had `-v`) every line is kept, which is what
/// `go test -v` prints. Without it, as `go test` does, passing and skipped
/// tests print nothing and a failing test prints its `--- FAIL` line, then
/// its log lines and failing subtests in the order they happened, each
/// subtest indented four more spaces. The final `FAIL` that the `go`
/// command adds after a failing run is not in the stream and is added back.
pub(super) fn render(output: &str, verbose: bool) -> String {
    #[derive(Default)]
    struct Test {
        header: Option<String>,
        items: Vec<Item>,
    }
    enum Item {
        Log(String),
        Subtest(String),
    }
    fn write(
        tests: &HashMap<(String, String), Test>,
        key: &(String, String),
        depth: usize,
        text: &mut String,
    ) {
        let Some(test) = tests.get(key) else { return };
        let indent = "    ".repeat(depth);
        if let Some(header) = &test.header {
            text.push_str(&indent);
            text.push_str(header);
            text.push('\n');
        }
        for item in &test.items {
            match item {
                Item::Log(line) => {
                    text.push_str(&indent);
                    text.push_str(line);
                }
                Item::Subtest(name) => {
                    write(tests, &(key.0.clone(), name.clone()), depth + 1, text)
                }
            }
        }
    }

    let mut text = String::new();
    let mut tests: HashMap<(String, String), Test> = HashMap::new();
    let mut failed = false;
    for line in lines(output) {
        let event = match line {
            Line::Text(line) => {
                text.push_str(line);
                text.push('\n');
                continue;
            }
            Line::Event(event) => event,
        };
        if event.test.is_none() && matches!(event.action.as_str(), "fail" | "build-fail") {
            failed = true;
        }
        let output = event.output.as_deref().unwrap_or("");
        if verbose || event.action == "build-output" {
            text.push_str(output);
            continue;
        }
        let Some(name) = event.test else {
            if output != "PASS\n" {
                text.push_str(output);
            }
            continue;
        };
        let package = event.package.unwrap_or_default();
        let key = (package.clone(), name.clone());
        match event.action.as_str() {
            "output" => {
                let test = tests.entry(key).or_default();
                let trimmed = output.trim();
                if ["--- FAIL", "--- PASS", "--- SKIP"]
                    .iter()
                    .any(|p| trimmed.starts_with(p))
                {
                    test.header = Some(trimmed.to_string());
                } else if !is_progress(output) {
                    test.items.push(Item::Log(output.to_string()));
                }
            }
            "fail" => {
                let parent = name
                    .rsplit_once('/')
                    .map(|(parent, _)| (package.clone(), parent.to_string()))
                    .filter(|parent| tests.contains_key(parent));
                match parent {
                    Some(parent) => {
                        if let Some(parent) = tests.get_mut(&parent) {
                            parent.items.push(Item::Subtest(name));
                        }
                    }
                    None => write(&tests, &key, 0, &mut text),
                }
            }
            _ => {}
        }
    }
    if failed && !verbose {
        text.push_str("FAIL\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `go test -json ./...` over one passing package and one failing one.
    const STREAM: &str = r#"{"Time":"2026-10-13T10:00:00Z","Action":"start","Package":"example.com/m/a"}
{"Action":"run","Package":"example.com/m/a","Test":"TestAdd"}
{"Action":"output","Package":"example.com/m/a","Test":"TestAdd","Output":"=== RUN   TestAdd\n"}
{"Action":"output","Package":"example.com/m/a","Test":"TestAdd","Output":"--- PASS: TestAdd (0.00s)\n"}
{"Action":"pass","Package":"example.com/m/a","Test":"TestAdd","Elapsed":0.001}
{"Action":"output","Package":"example.com/m/a","Output":"PASS\n"}
{"Action":"output","Package":"example.com/m/a","Output":"ok  \texample.com/m/a\t0.010s\n"}
{"Action":"pass","Package":"example.com/m/a","Elapsed":0.01}
{"Action":"run","Package":"example.com/m/b","Test":"TestDiv"}
{"Action":"output","Package":"example.com/m/b","Test":"TestDiv","Output":"=== RUN   TestDiv\n"}
{"Action":"run","Package":"example.com/m/b","Test":"TestDiv/by_zero"}
{"Action":"output","Package":"example.com/m/b","Test":"TestDiv/by_zero","Output":"=== RUN   TestDiv/by_zero\n"}
{"Action":"output","Package":"example.com/m/b","Test":"TestDiv/by_zero","Output":"    div_test.go:12: expected error\n"}
{"Action":"output","Package":"example.com/m/b","Test":"TestDiv/by_zero","Output":"    --- FAIL: TestDiv/by_zero (0.00s)\n"}
{"Action":"fail","Package":"example.com/m/b","Test":"TestDiv/by_zero","Elapsed":0.002}
{"Action":"output","Package":"example.com/m/b","Test":"TestDiv","Output":"--- FAIL: TestDiv (0.00s)\n"}
{"Action":"fail","Package":"example.com/m/b","Test":"TestDiv","Elapsed":0.003}
{"Action":"run","Package":"example.com/m/b","Test":"TestSkip"}
{"Action":"output","Package":"example.com/m/b","Test":"TestSkip","Output":"--- SKIP: TestSkip (0.00s)\n"}
{"Action":"skip","Package":"example.com/m/b","Test":"TestSkip","Elapsed":0}
{"Action":"output","Package":"example.com/m/b","Output":"FAIL\n"}
{"Action":"output","Package":"example.com/m/b","Output":"FAIL\texample.com/m/b\t0.020s\n"}
{"Action":"fail","Package":"example.com/m/b","Elapsed":0.02}
"#;

    #[test]
    fn reads_tests_and_subtests() {
        let parsed = parse(STREAM);
        assert!(parsed.complete);
        let cases: Vec<_> = parsed
            .cases
            .iter()
            .map(|c| (c.suite.as_deref().unwrap(), c.name.as_str(), c.outcome))
            .collect();
        assert_eq!(
            cases,
            vec![
                ("example.com/m/a", "TestAdd", TestOutcome::Pass),
                ("example.com/m/b", "TestDiv", TestOutcome::Fail),
                ("example.com/m/b", "TestDiv/by_zero", TestOutcome::Fail),
                ("example.com/m/b", "TestSkip", TestOutcome::Skip),
            ]
        );
        let sub = &parsed.cases[2];
        assert_eq!(sub.duration_ms, Some(2));
        assert_eq!(
            sub.message.as_deref(),
            Some("div_test.go:12: expected error\n    --- FAIL: TestDiv/by_zero (0.00s)")
        );
        assert_eq!(parsed.cases[0].message, None);
    }

    #[test]
    fn build_failure_is_incomplete_and_records_no_tests() {
        let stream = r#"{"ImportPath":"example.com/m/c","Action":"build-output","Output":"c/c.go:3:1: syntax error\n"}
{"ImportPath":"example.com/m/c","Action":"build-fail"}
{"Action":"start","Package":"example.com/m/c"}
{"Action":"output","Package":"example.com/m/c","Output":"FAIL\texample.com/m/c [build failed]\n"}
{"Action":"fail","Package":"example.com/m/c","Elapsed":0}
"#;
        let parsed = parse(stream);
        assert!(parsed.cases.is_empty());
        assert!(!parsed.complete);
    }

    #[test]
    fn a_test_cut_off_by_a_crash_is_an_error() {
        let stream = r#"{"Action":"run","Package":"p","Test":"TestHang"}
{"Action":"output","Package":"p","Test":"TestHang","Output":"panic: test timed out after 10m0s\n"}
{"Action":"output","Package":"p","Output":"FAIL\tp\t600.1s\n"}
{"Action":"fail","Package":"p","Elapsed":600.1}
"#;
        let parsed = parse(stream);
        assert!(!parsed.complete);
        assert_eq!(parsed.cases[0].outcome, TestOutcome::Error);
        let message = parsed.cases[0].message.as_deref().unwrap();
        assert!(message.starts_with("panic: test timed out"));
        assert!(message.ends_with("before the test binary exited."));
    }

    #[test]
    fn renders_like_go_test_without_v() {
        assert_eq!(
            render(STREAM, false),
            "ok  \texample.com/m/a\t0.010s\n\
             --- FAIL: TestDiv (0.00s)\n\
             \x20   --- FAIL: TestDiv/by_zero (0.00s)\n\
             \x20       div_test.go:12: expected error\n\
             FAIL\n\
             FAIL\texample.com/m/b\t0.020s\n\
             FAIL\n"
        );
    }

    /// Recorded from `go test -json ./...` (Go 1.25). Without `-json` the
    /// same run printed exactly this, with its own timings.
    #[test]
    fn renders_a_real_stream_as_go_test_printed_it() {
        let stream = include_str!("testdata/go-test.json");
        assert_eq!(
            render(stream, false),
            "ok  \texample.com/m\t1.825s\n\
             --- FAIL: TestDiv (0.00s)\n\
             \x20   --- FAIL: TestDiv/by_zero (0.00s)\n\
             \x20       b_test.go:6: expected error\n\
             FAIL\n\
             FAIL\texample.com/m/b\t1.960s\n\
             FAIL\n"
        );
        let parsed = parse(stream);
        assert!(parsed.complete);
        assert_eq!(parsed.cases.len(), 4);
    }

    #[test]
    fn renders_everything_with_v() {
        let text = render(STREAM, true);
        assert!(text.contains("=== RUN   TestAdd\n--- PASS: TestAdd (0.00s)\nPASS\n"));
        assert!(text.contains("--- SKIP: TestSkip"));
        assert!(!text.ends_with("FAIL\nFAIL\n"));
    }

    #[test]
    fn keeps_plain_text_lines() {
        let stream = "# example.com/m/c\nc/c.go:3:1: syntax error\n{\"Action\":\"output\",\"Package\":\"c\",\"Output\":\"FAIL\\tc [build failed]\\n\"}\n";
        assert_eq!(
            render(stream, false),
            "# example.com/m/c\nc/c.go:3:1: syntax error\nFAIL\tc [build failed]\n"
        );
    }
}
