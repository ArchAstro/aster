//! ExUnit results.
//!
//! The main source is the JSON lines written by Aster's own formatter,
//! `exunit_formatter.exs`, which Aster loads into `mix test`. Those results
//! are complete only when they agree with the summary ExUnit prints
//! (`4 tests, 1 failure`), so a test the formatter missed, or a run that
//! crashed before its summary, is never taken for a full picture.
//!
//! Without the formatter (the project configures its own formatters, or
//! loading it failed) the failures are read from ExUnit's console output,
//! which names only failing tests, so those results are always partial.

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use super::{Collected, TestCase, TestOutcome};

/// The formatter loaded into `mix test` with `elixir -r`.
pub(crate) const FORMATTER_SOURCE: &str = include_str!("exunit_formatter.exs");
/// Its module name, passed to `mix test --formatter`.
pub(crate) const FORMATTER_MODULE: &str = "AsterTestResults.ExUnitFormatter";
/// The environment variable naming the file it writes.
pub(crate) const REPORT_ENV: &str = "ASTER_EXUNIT_REPORT";

#[derive(Deserialize)]
struct Line {
    file: Option<String>,
    suite: Option<String>,
    name: String,
    outcome: String,
    duration_ms: Option<u64>,
    message: Option<String>,
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());
/// `1 doctest, 4 tests, 1 failure, 2 invalid, 1 skipped (3 excluded)`.
static SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+ (?:doctests?|properties|property|tests?)\b.*?(\d+) failures?").unwrap()
});
static INVALID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+) invalid").unwrap());
/// `  1) test creates an agent (Platform.AgentsTest)`.
static FAILURE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+\d+\) (.+) \(([A-Za-z0-9_.]+)\)$").unwrap());
static LOCATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+(\S+\.exs?):\d+$").unwrap());

/// Results from the formatter's report, or from the console output when
/// there is no report.
pub(super) fn collect(report: Option<&str>, stdout: Option<&str>) -> Collected {
    let stdout = stdout.map(|s| ANSI.replace_all(s, "").into_owned());
    let Some(report) = report else {
        return Collected {
            cases: stdout.as_deref().map(console_failures).unwrap_or_default(),
            complete: false,
        };
    };
    let mut complete = true;
    let mut cases = Vec::new();
    for line in report.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            complete = false;
            continue;
        };
        let outcome = match line.outcome.as_str() {
            "pass" => TestOutcome::Pass,
            "fail" => TestOutcome::Fail,
            "error" => TestOutcome::Error,
            "skip" => TestOutcome::Skip,
            _ => {
                complete = false;
                continue;
            }
        };
        cases.push(TestCase {
            file: line.file.map(|f| f.replace('\\', "/")),
            suite: line.suite,
            name: line.name,
            outcome,
            attempt: 1,
            duration_ms: line.duration_ms,
            message: line.message.filter(|_| outcome.is_failure()),
        });
    }
    let failed = cases.iter().filter(|c| c.outcome.is_failure()).count();
    complete &= stdout
        .as_deref()
        .and_then(summary_failures)
        .is_some_and(|reported| reported == failed);
    Collected { cases, complete }
}

/// Failures plus invalid tests across every summary line (an umbrella
/// prints one per app). `None` when there is no summary, as when the run
/// crashed.
fn summary_failures(stdout: &str) -> Option<usize> {
    let mut total = None;
    for line in stdout.lines() {
        let line = line.trim();
        let Some(captures) = SUMMARY.captures(line) else {
            continue;
        };
        let failures: usize = captures[1].parse().ok()?;
        let invalid: usize = INVALID
            .captures(line)
            .and_then(|c| c[1].parse().ok())
            .unwrap_or(0);
        *total.get_or_insert(0) += failures + invalid;
    }
    total
}

/// Failed tests from ExUnit's console failure blocks:
///
/// ```text
///   1) test fails (DemoTest)
///      test/demo_test.exs:4
///      Assertion with == failed
/// ```
fn console_failures(stdout: &str) -> Vec<TestCase> {
    let lines: Vec<&str> = stdout.lines().collect();
    let mut cases = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(header) = FAILURE_HEADER.captures(lines[i]) else {
            i += 1;
            continue;
        };
        let start = i;
        let file = lines
            .get(i + 1)
            .and_then(|line| LOCATION.captures(line))
            .map(|c| c[1].to_string());
        i += 1;
        // The block runs until the next failure, a blank line followed by
        // unindented text, or the summary.
        while i < lines.len()
            && !FAILURE_HEADER.is_match(lines[i])
            && !(lines[i].trim().is_empty()
                && lines.get(i + 1).is_some_and(|next| !next.starts_with(' ')))
        {
            i += 1;
        }
        cases.push(TestCase {
            file,
            suite: Some(header[2].to_string()),
            name: header[1].to_string(),
            outcome: TestOutcome::Fail,
            attempt: 1,
            duration_ms: None,
            message: Some(lines[start..i].join("\n").trim_end().to_string()),
        });
    }
    cases
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"{"file":"test/demo_test.exs","suite":"DemoTest","name":"test passes","outcome":"pass","duration_ms":0}
{"file":"test/demo_test.exs","suite":"DemoTest","name":"test fails","outcome":"fail","duration_ms":3,"message":"  1) test fails (DemoTest)\n     test/demo_test.exs:4\n     Assertion with == failed"}
{"file":"test/demo_test.exs","suite":"DemoTest","name":"test skipped","outcome":"skip","duration_ms":0}
"#;

    #[test]
    fn report_agreeing_with_the_summary_is_complete() {
        let collected = collect(
            Some(REPORT),
            Some("Running ExUnit\n\nFinished in 0.02 seconds\n3 tests, 1 failure, 1 skipped\n"),
        );
        assert!(collected.complete);
        let outcomes: Vec<_> = collected.cases.iter().map(|c| c.outcome).collect();
        assert_eq!(
            outcomes,
            vec![TestOutcome::Pass, TestOutcome::Fail, TestOutcome::Skip]
        );
        assert_eq!(collected.cases[1].duration_ms, Some(3));
        assert!(collected.cases[1]
            .message
            .as_deref()
            .unwrap()
            .contains("Assertion with == failed"));
    }

    #[test]
    fn report_disagreeing_with_the_summary_is_partial() {
        let collected = collect(Some(REPORT), Some("3 tests, 2 failures\n"));
        assert!(!collected.complete);
    }

    #[test]
    fn missing_summary_is_partial() {
        let collected = collect(Some(REPORT), Some("** (EXIT) killed\n"));
        assert!(!collected.complete);
        assert_eq!(collected.cases.len(), 3);
    }

    #[test]
    fn summaries_add_up_across_umbrella_apps_and_count_invalid_tests() {
        let report = REPORT.to_string()
            + r#"{"suite":"Other","name":"test x","outcome":"error","message":"setup_all failed for this module"}"#;
        let stdout =
            "\x1b[31m2 tests, 1 failure\x1b[0m\n...\n1 doctest, 2 tests, 0 failures, 1 invalid\n";
        assert!(collect(Some(&report), Some(stdout)).complete);
    }

    #[test]
    fn bad_report_lines_make_it_partial() {
        let report = format!("{REPORT}not json\n");
        assert!(!collect(Some(&report), Some("3 tests, 1 failure\n")).complete);
    }

    /// Recorded from `mix test` (Elixir 1.18) without the formatter.
    const CONSOLE: &str = "\
Running ExUnit with seed: 1, max_cases: 20

.

  1) test fails (DemoTest)
     test/demo_test.exs:4
     Assertion with == failed
     code:  assert 1 + 1 == 3
     left:  2
     right: 3
     stacktrace:
       test/demo_test.exs:5: (test)

.
  2) test group inner raises (DemoTest.Nested)
     test/nested_test.exs:9
     ** (RuntimeError) boom
     stacktrace:
       test/nested_test.exs:10: (test)

Finished in 0.03 seconds (0.00s async, 0.03s sync)
4 tests, 2 failures, 1 skipped
";

    #[test]
    fn console_failures_without_a_report() {
        let collected = collect(None, Some(CONSOLE));
        assert!(!collected.complete, "console output never lists every test");
        assert_eq!(collected.cases.len(), 2);
        let first = &collected.cases[0];
        assert_eq!(first.name, "test fails");
        assert_eq!(first.suite.as_deref(), Some("DemoTest"));
        assert_eq!(first.file.as_deref(), Some("test/demo_test.exs"));
        let message = first.message.as_deref().unwrap();
        assert!(message.starts_with("  1) test fails (DemoTest)"));
        assert!(message.ends_with("test/demo_test.exs:5: (test)"));
        let second = &collected.cases[1];
        assert_eq!(second.name, "test group inner raises");
        assert_eq!(second.suite.as_deref(), Some("DemoTest.Nested"));
        assert!(!second.message.as_deref().unwrap().contains("Finished in"));
    }

    #[test]
    fn no_output_no_cases() {
        let collected = collect(None, None);
        assert!(collected.cases.is_empty());
        assert!(!collected.complete);
    }
}
