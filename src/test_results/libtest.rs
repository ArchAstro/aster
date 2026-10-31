//! `cargo test` console output.
//!
//! libtest prints `test <name> ... ok` per test on stdout, while cargo
//! names each test binary (`Running tests/api.rs (...)`, `Doc-tests demo`)
//! on stderr. Aster's log holds stdout then stderr, so the Nth `running N
//! tests` block belongs to the Nth binary cargo names; when the counts
//! disagree no suite is recorded. A test that prints without a newline can
//! break a result line, so the results are complete only when every binary's
//! `test result:` summary is there and the summaries add up to the tests
//! read.

use std::sync::LazyLock;

use regex::Regex;

use super::{Collected, TestCase, TestOutcome};

static RESULT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^test (.+?) \.\.\. (ok|FAILED|ignored\b.*)$").unwrap());
static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^running \d+ tests?$").unwrap());
static BINARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:Running (?:unittests )?(\S+)(?: \(.*\))?|Doc-tests (\S+))$").unwrap()
});
static SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured",
    )
    .unwrap()
});
static FAILURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---- (.+) (?:stdout|stderr) ----$").unwrap());

pub(super) fn parse(output: &str) -> Collected {
    let lines: Vec<&str> = output.lines().collect();

    let binaries: Vec<String> = lines
        .iter()
        .filter_map(|line| BINARY.captures(line))
        .map(|c| match c.get(1) {
            Some(path) => path.as_str().to_string(),
            None => "doctests".to_string(),
        })
        .collect();
    let blocks = lines.iter().filter(|line| BLOCK.is_match(line)).count();
    let suites_known = blocks == binaries.len();

    let mut cases = Vec::new();
    let mut block: Option<usize> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if BLOCK.is_match(line) {
            block = Some(block.map_or(0, |b| b + 1));
        } else if let Some(result) = RESULT.captures(line) {
            let outcome = match &result[2] {
                "ok" => TestOutcome::Pass,
                "FAILED" => TestOutcome::Fail,
                _ => TestOutcome::Skip,
            };
            cases.push(TestCase {
                file: None,
                suite: block
                    .filter(|_| suites_known)
                    .and_then(|b| binaries.get(b).cloned()),
                name: result[1].to_string(),
                outcome,
                attempt: 1,
                duration_ms: None,
                message: None,
            });
        } else if let Some(failure) = FAILURE.captures(line) {
            let name = failure[1].to_string();
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && !FAILURE.is_match(lines[end]) && lines[end] != "failures:" {
                end += 1;
            }
            let message = lines[start..end].join("\n").trim().to_string();
            if let Some(case) = cases
                .iter_mut()
                .rev()
                .find(|c| c.name == name && c.outcome == TestOutcome::Fail)
            {
                if !message.is_empty() {
                    case.message = Some(message);
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    // Every binary that started printed its summary, and the summaries
    // count exactly the results read: no line was lost or broken.
    let summaries: Vec<usize> = lines
        .iter()
        .filter_map(|line| SUMMARY.captures(line))
        .map(|c| (1..=4).map(|i| c[i].parse::<usize>().unwrap_or(0)).sum())
        .collect();
    let complete = !summaries.is_empty()
        && summaries.len() == blocks
        && summaries.iter().sum::<usize>() == cases.len();
    Collected { cases, complete }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from `cargo test --no-fail-fast` (Rust 1.95): stdout, then
    /// stderr.
    const OUTPUT: &str = include_str!("testdata/cargo-test.txt");

    #[test]
    fn reads_every_binary_and_names_its_suite() {
        let collected = parse(OUTPUT);
        assert!(collected.complete, "every summary adds up");
        let cases: Vec<_> = collected
            .cases
            .iter()
            .map(|c| (c.suite.as_deref(), c.name.as_str(), c.outcome))
            .collect();
        assert_eq!(
            cases,
            vec![
                (Some("src/lib.rs"), "tests::later", TestOutcome::Skip),
                (Some("src/lib.rs"), "tests::adds", TestOutcome::Pass),
                (Some("src/lib.rs"), "tests::divides", TestOutcome::Fail),
                (Some("tests/api.rs"), "adds", TestOutcome::Pass),
                (
                    Some("doctests"),
                    "src/lib.rs - add (line 3)",
                    TestOutcome::Pass
                ),
            ]
        );
        let message = collected.cases[2].message.as_deref().unwrap();
        assert!(message.starts_with("some captured output"), "{message}");
        assert!(message.contains("division is off"));
        assert!(!message.contains("failures:"));
    }

    #[test]
    fn a_missing_or_short_summary_makes_the_results_partial() {
        let ok = "running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert!(parse(ok).complete);
        // A binary that died before its summary.
        let died = format!("{ok}running 1 test\ntest c ... ok\n");
        assert!(!parse(&died).complete);
        // A result line broken by a test's own output.
        let broken = "running 2 tests\ntest a ... ok\nprinted test b ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert!(!parse(broken).complete);
    }

    #[test]
    fn without_cargo_lines_the_suite_is_unknown() {
        let collected = parse("running 1 test\ntest a::b ... ok\n");
        assert_eq!(collected.cases[0].suite, None);
        assert_eq!(collected.cases[0].outcome, TestOutcome::Pass);
    }

    #[test]
    fn ignored_with_a_reason_is_skipped() {
        let collected = parse("running 1 test\ntest slow ... ignored, needs network\n");
        assert_eq!(collected.cases[0].outcome, TestOutcome::Skip);
    }
}
