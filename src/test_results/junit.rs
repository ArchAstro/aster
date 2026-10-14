//! JUnit XML reports.
//!
//! One reader for every producer Aster meets: pytest, Gradle, Maven
//! Surefire and Failsafe, vitest, bun and nextest. They agree on
//! `<testsuite>`/`<testcase>` with `<failure>`, `<error>` and `<skipped>`
//! children and disagree on where the file and suite names go, so the file
//! comes from the test case or its nearest suite that names one, and the
//! suite from the test case's `classname` or else its suite's `name`.
//! Vitest names no file; its `classname` is the file's path, so a
//! `classname` with a JS or TS extension is taken as the file instead.
//!
//! Surefire's rerun elements become extra attempts: `<flakyFailure>` and
//! `<flakyError>` are failed attempts before a pass, `<rerunFailure>` and
//! `<rerunError>` are failed attempts after a failure.

use anyhow::Result;
use roxmltree::{Document, Node};

use super::{TestCase, TestOutcome};

pub(super) fn parse(content: &str) -> Result<Vec<TestCase>> {
    let document = Document::parse(content)?;
    let mut cases = Vec::new();
    for node in document.descendants() {
        if node.is_element() && node.tag_name().name() == "testcase" {
            read_case(node, &mut cases);
        }
    }
    Ok(cases)
}

/// Whether a classname is a source file path (`src/math.test.ts`) rather
/// than a module or class name (`tests.test_math`, `com.example.Test`).
fn is_source_path(name: &str) -> bool {
    const EXTENSIONS: [&str; 8] = ["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e))
}

fn read_case(node: Node, cases: &mut Vec<TestCase>) {
    let Some(name) = node.attribute("name") else {
        return;
    };
    let suites = || {
        node.ancestors()
            .skip(1)
            .filter(|n| n.is_element() && n.tag_name().name() == "testsuite")
    };
    let mut file = node
        .attribute("file")
        .or_else(|| suites().find_map(|s| s.attribute("file")))
        .map(normalize_file);
    let mut suite = node
        .attribute("classname")
        .filter(|c| !c.is_empty())
        .or_else(|| suites().find_map(|s| s.attribute("name")))
        .map(str::to_string);
    // Vitest names no file, but its classname is the test file's path.
    if file.is_none() && suite.as_deref().is_some_and(is_source_path) {
        file = suite.take().map(|path| normalize_file(&path));
    }
    let duration_ms = node.attribute("time").and_then(parse_seconds);

    let children: Vec<Node> = node.children().filter(|c| c.is_element()).collect();
    let child = |tag: &str| children.iter().find(|c| c.tag_name().name() == tag);
    let (outcome, message) = if let Some(failure) = child("failure") {
        (TestOutcome::Fail, message_of(failure))
    } else if let Some(error) = child("error") {
        (TestOutcome::Error, message_of(error))
    } else if child("skipped").is_some() {
        (TestOutcome::Skip, None)
    } else {
        (TestOutcome::Pass, None)
    };

    let case = |outcome, attempt, message, duration_ms| TestCase {
        file: file.clone(),
        suite: suite.clone(),
        name: name.to_string(),
        outcome,
        attempt,
        duration_ms,
        message,
    };

    let mut attempt = 1;
    if outcome == TestOutcome::Pass {
        for rerun in children
            .iter()
            .filter(|c| matches!(c.tag_name().name(), "flakyFailure" | "flakyError"))
        {
            let rerun_outcome = if rerun.tag_name().name() == "flakyFailure" {
                TestOutcome::Fail
            } else {
                TestOutcome::Error
            };
            cases.push(case(rerun_outcome, attempt, message_of(rerun), None));
            attempt += 1;
        }
    }
    cases.push(case(outcome, attempt, message, duration_ms));
    if outcome.is_failure() {
        for rerun in children
            .iter()
            .filter(|c| matches!(c.tag_name().name(), "rerunFailure" | "rerunError"))
        {
            attempt += 1;
            let rerun_outcome = if rerun.tag_name().name() == "rerunFailure" {
                TestOutcome::Fail
            } else {
                TestOutcome::Error
            };
            cases.push(case(rerun_outcome, attempt, message_of(rerun), None));
        }
    }
}

/// The element's `message` attribute and its text (often the stack trace),
/// joined by a blank line.
fn message_of(node: &Node) -> Option<String> {
    let attribute = node.attribute("message").map(str::trim).unwrap_or("");
    let text: String = node
        .descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect();
    let text = text.trim();
    let message = match (attribute.is_empty(), text.is_empty()) {
        (true, true) => return None,
        (false, true) => attribute.to_string(),
        (true, false) => text.to_string(),
        (false, false) if text.starts_with(attribute) => text.to_string(),
        (false, false) => format!("{attribute}\n\n{text}"),
    };
    Some(message)
}

fn parse_seconds(value: &str) -> Option<u64> {
    // Some producers group thousands ("1,234.5").
    let seconds: f64 = value.replace(',', "").trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).round() as u64)
}

fn normalize_file(file: &str) -> String {
    file.replace('\\', "/").trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcomes(cases: &[TestCase]) -> Vec<(&str, TestOutcome, u32)> {
        cases
            .iter()
            .map(|c| (c.name.as_str(), c.outcome, c.attempt))
            .collect()
    }

    #[test]
    fn pytest_report() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites><testsuite name="pytest" errors="1" failures="1" skipped="1" tests="4" time="0.2">
<testcase classname="tests.test_math" name="test_add" time="0.001"/>
<testcase classname="tests.test_math.TestDiv" name="test_zero" time="0.010">
  <failure message="ZeroDivisionError: division by zero">def test_zero():
&gt;       1 / 0
E       ZeroDivisionError: division by zero</failure>
</testcase>
<testcase classname="tests.test_math" name="test_skip" time="0.000"><skipped type="pytest.skip" message="later">skipped</skipped></testcase>
<testcase classname="tests.test_io" name="test_fixture" time="0.002"><error message="failed on setup with &quot;KeyError&quot;">KeyError</error></testcase>
</testsuite></testsuites>"#;
        let cases = parse(xml).unwrap();
        assert_eq!(
            outcomes(&cases),
            vec![
                ("test_add", TestOutcome::Pass, 1),
                ("test_zero", TestOutcome::Fail, 1),
                ("test_skip", TestOutcome::Skip, 1),
                ("test_fixture", TestOutcome::Error, 1),
            ]
        );
        assert_eq!(cases[1].suite.as_deref(), Some("tests.test_math.TestDiv"));
        assert_eq!(cases[1].duration_ms, Some(10));
        let message = cases[1].message.as_deref().unwrap();
        assert!(message.starts_with("ZeroDivisionError: division by zero\n\n"));
        assert!(message.contains(">       1 / 0"));
        assert_eq!(cases[2].message, None);
        assert_eq!(cases[0].file, None);
    }

    #[test]
    fn gradle_report() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.example.CalculatorTest" tests="2" skipped="0" failures="1" errors="0" timestamp="2026-10-13T10:00:00" hostname="ci" time="0.031">
  <properties/>
  <testcase name="adds()" classname="com.example.CalculatorTest" time="0.004"/>
  <testcase name="divides()" classname="com.example.CalculatorTest" time="0.027">
    <failure message="org.opentest4j.AssertionFailedError: expected: &lt;2&gt; but was: &lt;3&gt;" type="org.opentest4j.AssertionFailedError">org.opentest4j.AssertionFailedError: expected: &lt;2&gt; but was: &lt;3&gt;
	at com.example.CalculatorTest.divides(CalculatorTest.java:14)</failure>
  </testcase>
  <system-out><![CDATA[]]></system-out>
  <system-err><![CDATA[]]></system-err>
</testsuite>"#;
        let cases = parse(xml).unwrap();
        assert_eq!(
            outcomes(&cases),
            vec![
                ("adds()", TestOutcome::Pass, 1),
                ("divides()", TestOutcome::Fail, 1)
            ]
        );
        // The text repeats the message, so it is not doubled.
        let message = cases[1].message.as_deref().unwrap();
        assert!(message.starts_with("org.opentest4j.AssertionFailedError: expected: <2>"));
        assert_eq!(message.matches("expected: <2>").count(), 1);
    }

    #[test]
    fn surefire_flaky_and_rerun_failures_become_attempts() {
        let xml = r#"<testsuite name="com.example.FlakyTest">
  <testcase name="sometimes" classname="com.example.FlakyTest" time="0.2">
    <flakyFailure message="first" type="java.lang.AssertionError">trace one</flakyFailure>
    <flakyError message="second" type="java.io.IOException">trace two</flakyError>
  </testcase>
  <testcase name="never" classname="com.example.FlakyTest" time="0.1">
    <failure message="nope" type="java.lang.AssertionError"/>
    <rerunFailure message="nope again" type="java.lang.AssertionError"/>
  </testcase>
</testsuite>"#;
        let cases = parse(xml).unwrap();
        assert_eq!(
            outcomes(&cases),
            vec![
                ("sometimes", TestOutcome::Fail, 1),
                ("sometimes", TestOutcome::Error, 2),
                ("sometimes", TestOutcome::Pass, 3),
                ("never", TestOutcome::Fail, 1),
                ("never", TestOutcome::Fail, 2),
            ]
        );
        assert_eq!(cases[0].message.as_deref(), Some("first\n\ntrace one"));
        assert_eq!(cases[2].duration_ms, Some(200));
        assert_eq!(cases[4].message.as_deref(), Some("nope again"));
    }

    #[test]
    fn vitest_report_takes_the_file_from_the_suite() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<testsuites name="vitest tests" tests="2" failures="1" errors="0" time="0.5">
    <testsuite name="src/math.test.ts" timestamp="2026-10-13T10:00:00" hostname="ci" tests="2" failures="1" errors="0" skipped="0" time="0.01">
        <testcase classname="src/math.test.ts" name="math &gt; adds" time="0.001">
        </testcase>
        <testcase classname="src/math.test.ts" name="math &gt; subtracts" time="0.002">
            <failure message="expected 1 to be 2 // Object.is equality" type="AssertionError">
AssertionError: expected 1 to be 2 // Object.is equality
 ❯ src/math.test.ts:9:17
            </failure>
        </testcase>
    </testsuite>
</testsuites>"#;
        let cases = parse(xml).unwrap();
        assert_eq!(cases[0].name, "math > adds");
        assert_eq!(cases[1].outcome, TestOutcome::Fail);
        assert_eq!(cases[1].file.as_deref(), Some("src/math.test.ts"));
        assert_eq!(cases[1].suite, None);
        assert!(cases[1]
            .message
            .as_deref()
            .unwrap()
            .contains("src/math.test.ts:9:17"));
    }

    #[test]
    fn bun_report_with_nested_suites_and_file_attribute() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="bun test" tests="1" failures="0">
  <testsuite name="test/a.test.ts" file="./test/a.test.ts" tests="1">
    <testsuite name="group" file="./test/a.test.ts" line="3" tests="1">
      <testcase name="works" classname="group" time="0.0004" file="./test/a.test.ts" line="4"/>
    </testsuite>
  </testsuite>
</testsuites>"#;
        let cases = parse(xml).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].file.as_deref(), Some("test/a.test.ts"));
        assert_eq!(cases[0].suite.as_deref(), Some("group"));
        assert_eq!(cases[0].duration_ms, Some(0));
    }

    #[test]
    fn nextest_report_falls_back_to_the_suite_name() {
        let xml = r#"<testsuites name="nextest-run"><testsuite name="aster::integration" tests="1">
<testcase name="cache::hits" classname="" time="1,234.5"/></testsuite></testsuites>"#;
        let cases = parse(xml).unwrap();
        assert_eq!(cases[0].suite.as_deref(), Some("aster::integration"));
        assert_eq!(cases[0].duration_ms, Some(1_234_500));
    }

    #[test]
    fn testcase_without_a_name_is_ignored() {
        let cases = parse(r#"<testsuite><testcase classname="A"/></testsuite>"#).unwrap();
        assert!(cases.is_empty());
    }

    #[test]
    fn malformed_xml_is_an_error() {
        assert!(parse("<testsuite><testcase").is_err());
    }
}
