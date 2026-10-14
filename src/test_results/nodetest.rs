//! Node's built-in test runner (`node --test`), read from the JSON lines
//! Aster's reporter, `node_test_reporter.mjs`, writes.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{Collected, TestCase, TestOutcome};

/// The reporter Aster loads through `NODE_OPTIONS`.
pub(crate) const REPORTER_SOURCE: &str = include_str!("node_test_reporter.mjs");
/// The environment variable naming the file it writes.
pub(crate) const REPORT_ENV: &str = "ASTER_NODE_TEST_REPORT";

#[derive(Deserialize)]
struct Line {
    file: Option<String>,
    name: String,
    outcome: String,
    duration_ms: Option<f64>,
    message: Option<String>,
}

/// Each line is one test, named by its enclosing suites and tests and its
/// own name, `outer > inner > name`, the way Jest's results are. A file
/// that fails to load is reported as one errored test named after it.
pub(super) fn parse(content: &str) -> Result<Collected> {
    let mut cases = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line: Line = serde_json::from_str(line)
            .with_context(|| format!("line {} is not a test result", index + 1))?;
        let outcome = match line.outcome.as_str() {
            "pass" => TestOutcome::Pass,
            "fail" => TestOutcome::Fail,
            "error" => TestOutcome::Error,
            _ => TestOutcome::Skip,
        };
        cases.push(TestCase {
            file: line.file,
            suite: None,
            name: line.name,
            outcome,
            attempt: 1,
            duration_ms: line
                .duration_ms
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .map(|ms| ms.round() as u64),
            message: line.message.filter(|_| outcome.is_failure()),
        });
    }
    Ok(Collected {
        cases,
        complete: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_each_line_as_a_test() {
        let content = concat!(
            r#"{"file":"/p/a.test.mjs","name":"suite A > passes","outcome":"pass","duration_ms":0.7,"message":null}"#,
            "\n",
            r#"{"file":"/p/a.test.mjs","name":"suite A > fails","outcome":"fail","duration_ms":1.5,"message":"AssertionError: 1 == 2"}"#,
            "\n\n",
            r#"{"file":"/p/c.test.mjs","name":"/p/c.test.mjs","outcome":"error","duration_ms":null,"message":"Error: boom"}"#,
            "\n",
            r#"{"file":"/p/a.test.mjs","name":"skipped","outcome":"skip","duration_ms":0.0,"message":"ignored"}"#,
            "\n",
        );
        let collected = parse(content).unwrap();
        assert!(collected.complete);
        let summary: Vec<_> = collected
            .cases
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.outcome,
                    c.duration_ms,
                    c.message.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("suite A > passes", TestOutcome::Pass, Some(1), None),
                (
                    "suite A > fails",
                    TestOutcome::Fail,
                    Some(2),
                    Some("AssertionError: 1 == 2")
                ),
                (
                    "/p/c.test.mjs",
                    TestOutcome::Error,
                    None,
                    Some("Error: boom")
                ),
                ("skipped", TestOutcome::Skip, Some(0), None),
            ]
        );
        assert_eq!(collected.cases[0].file.as_deref(), Some("/p/a.test.mjs"));
    }

    #[test]
    fn a_line_that_is_not_a_result_is_an_error() {
        assert!(parse("not json\n").is_err());
    }
}
