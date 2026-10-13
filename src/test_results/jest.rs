//! Jest's `--json` results.

use anyhow::Result;
use serde::Deserialize;

use super::{Collected, TestCase, TestOutcome};

#[derive(Deserialize)]
struct Report {
    #[serde(rename = "testResults", default)]
    test_results: Vec<Suite>,
}

#[derive(Deserialize)]
struct Suite {
    /// The test file, by absolute path.
    name: String,
    #[serde(rename = "assertionResults", default)]
    assertions: Vec<Assertion>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct Assertion {
    #[serde(rename = "ancestorTitles", default)]
    ancestors: Vec<String>,
    title: String,
    status: String,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(rename = "failureMessages", default)]
    failure_messages: Vec<String>,
}

/// Each assertion becomes a test named by its describe blocks and title,
/// `outer > inner > title`, the way Vitest's JUnit report names them. A file
/// that failed without running its tests (a syntax error, a failing import)
/// has none to report, so the results are incomplete.
pub(super) fn parse(content: &str) -> Result<Collected> {
    let report: Report = serde_json::from_str(content)?;
    let mut complete = true;
    let mut cases = Vec::new();
    for suite in report.test_results {
        if suite.assertions.is_empty() && suite.status.as_deref() == Some("failed") {
            complete = false;
        }
        for assertion in suite.assertions {
            let outcome = match assertion.status.as_str() {
                "passed" => TestOutcome::Pass,
                "failed" => TestOutcome::Fail,
                _ => TestOutcome::Skip,
            };
            let mut name = assertion.ancestors.join(" > ");
            if !name.is_empty() {
                name.push_str(" > ");
            }
            name.push_str(&assertion.title);
            cases.push(TestCase {
                file: Some(suite.name.clone()),
                suite: None,
                name,
                outcome,
                attempt: 1,
                duration_ms: assertion
                    .duration
                    .filter(|ms| ms.is_finite() && *ms >= 0.0)
                    .map(|ms| ms.round() as u64),
                message: (outcome.is_failure() && !assertion.failure_messages.is_empty())
                    .then(|| assertion.failure_messages.join("\n")),
            });
        }
    }
    Ok(Collected { cases, complete })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"{
  "numFailedTests": 1,
  "success": false,
  "testResults": [
    {
      "name": "/work/app/src/math.test.js",
      "status": "failed",
      "message": "",
      "assertionResults": [
        {"ancestorTitles": ["math", "add"], "fullName": "math add works", "title": "works", "status": "passed", "duration": 3, "failureMessages": []},
        {"ancestorTitles": ["math"], "fullName": "math divides", "title": "divides", "status": "failed", "duration": 1.6, "failureMessages": ["Error: expect(received).toBe(expected)\n\nExpected: 2\nReceived: 3"]},
        {"ancestorTitles": [], "fullName": "later", "title": "later", "status": "todo", "duration": null, "failureMessages": []}
      ]
    }
  ]
}"#;

    #[test]
    fn reads_assertions() {
        let collected = parse(REPORT).unwrap();
        assert!(collected.complete);
        let names: Vec<_> = collected
            .cases
            .iter()
            .map(|c| (c.name.as_str(), c.outcome))
            .collect();
        assert_eq!(
            names,
            vec![
                ("math > add > works", TestOutcome::Pass),
                ("math > divides", TestOutcome::Fail),
                ("later", TestOutcome::Skip),
            ]
        );
        assert_eq!(
            collected.cases[1].file.as_deref(),
            Some("/work/app/src/math.test.js")
        );
        assert_eq!(collected.cases[1].duration_ms, Some(2));
        assert!(collected.cases[1]
            .message
            .as_deref()
            .unwrap()
            .contains("Received: 3"));
        assert_eq!(collected.cases[0].message, None);
    }

    #[test]
    fn a_file_that_failed_to_run_makes_it_incomplete() {
        let report = r#"{"testResults":[{"name":"/w/broken.test.js","status":"failed","message":"SyntaxError","assertionResults":[]}]}"#;
        let collected = parse(report).unwrap();
        assert!(!collected.complete);
        assert!(collected.cases.is_empty());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse("{").is_err());
    }
}
