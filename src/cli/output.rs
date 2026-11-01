//! Output formatting and JSON helpers
//!
//! Provides OutputMode enum for controlling output format and helpers
//! for JSON serialization in CLI commands.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Write};

use crate::executor::ExecutionResult;

/// Output mode for CLI commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Normal output (default)
    Normal,
    /// Verbose output with diagnostics
    Verbose,
    /// Quiet output (minimal)
    Quiet,
    /// JSON output for machine consumption
    Json,
}

/// Output JSON to stdout
///
/// Uses pretty printing if stdout is a terminal, compact otherwise.
/// Always ensures trailing newline.
pub fn output_json<T: Serialize>(value: &T) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    if console::Term::stdout().is_term() {
        serde_json::to_writer_pretty(&mut handle, value)?;
    } else {
        serde_json::to_writer(&mut handle, value)?;
    }

    writeln!(handle)?;
    Ok(())
}

/// Project information for JSON output (list command)
#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    /// Project address (e.g., "//services/api")
    pub address: String,
    /// Relative path from workspace root
    pub path: String,
    /// Internal plugin that discovered and executes this project
    pub plugin: String,
    /// Source languages used by the project
    pub languages: Vec<String>,
    /// Build system used by the project, when distinct from its language
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_system: Option<String>,
    /// Available targets and their commands
    pub targets: HashMap<String, String>,
}

/// Graph output for JSON (graph command)
#[derive(Debug, Clone, Serialize)]
pub struct GraphOutput {
    /// All target addresses in the graph
    pub nodes: Vec<String>,
    /// Adjacency list: target -> dependencies
    pub edges: HashMap<String, Vec<String>>,
    /// Target -> workspace-relative globs it reads outside its project
    /// (`[targets.<name>.cache] workspace_include`). These are file-level
    /// dependencies the edges cannot show; a sparse checkout needs them.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub workspace_inputs: HashMap<String, Vec<String>>,
    /// Target -> targets whose runs it joins (`selected_by`). Not edges:
    /// they order nothing and select nothing in the other direction.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub selected_by: HashMap<String, Vec<String>>,
}

/// Why command output for JSON
#[derive(Debug, Clone, Serialize)]
pub struct WhyOutput {
    /// Source target
    pub from: String,
    /// Destination target
    pub to: String,
    /// Path from source to destination (None if no path found)
    pub path: Option<Vec<String>>,
}

/// Execution output for JSON (run/affected commands)
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionOutput {
    /// Results per target
    pub results: Vec<TargetResult>,
    /// Execution summary
    pub summary: ExecutionSummary,
}

/// Result of executing a single target
#[derive(Debug, Clone, Serialize)]
pub struct TargetResult {
    /// Target address
    pub address: String,
    /// Execution status
    pub status: String,
    /// Whether result was from cache
    pub cached: bool,
    /// Exit code (None if skipped). A cache hit reports the stored code.
    pub exit_code: Option<i32>,
    /// Duration in milliseconds
    pub duration_ms: u128,
    /// Output (stdout + stderr)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Per-test results, when the run recorded them
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestSummary>,
}

/// The tests one target ran, from `--test-results`
#[derive(Debug, Clone, Serialize)]
pub struct TestSummary {
    /// How the results were captured (e.g. "junit", "go-json")
    pub capture: String,
    /// "complete", or "partial" when some tests may be missing
    pub confidence: aster_results_model::Confidence,
    /// Test attempts reported
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// The attempts that failed or errored
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<FailedTest>,
    /// The tests that failed and then passed on a retry
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flaky: Vec<FlakyTestSummary>,
    /// Failing tests the quarantine let through: the target passed
    /// because of them
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quarantined: Vec<crate::test_results::quarantine::Quarantined>,
}

/// One test that passed only on a retry
#[derive(Debug, Clone, Serialize)]
pub struct FlakyTestSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The attempt that passed (2 for the first retry)
    pub passed_on_attempt: u32,
}

/// One failed test attempt
#[derive(Debug, Clone, Serialize)]
pub struct FailedTest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub attempt: u32,
}

fn test_summary(report: &crate::test_results::TestReport) -> TestSummary {
    use crate::test_results::TestOutcome;
    let count = |outcome: TestOutcome| {
        report
            .cases
            .iter()
            .filter(|case| case.outcome == outcome)
            .count()
    };
    TestSummary {
        capture: report.capture.clone(),
        confidence: report.confidence,
        total: report.cases.len(),
        passed: count(TestOutcome::Pass),
        failed: report.failures(),
        skipped: count(TestOutcome::Skip),
        failures: report
            .cases
            .iter()
            .filter(|case| case.outcome.is_failure())
            .map(|case| FailedTest {
                name: case.name.clone(),
                suite: case.suite.clone(),
                file: case.file.clone(),
                attempt: case.attempt,
            })
            .collect(),
        flaky: report
            .flaky()
            .into_iter()
            .map(|test| FlakyTestSummary {
                name: test.name,
                suite: test.suite,
                file: test.file,
                passed_on_attempt: test.passed_on_attempt,
            })
            .collect(),
        quarantined: report.quarantined.clone(),
    }
}

/// Summary of execution results
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionSummary {
    /// Total number of targets
    pub total: usize,
    /// Number of targets that passed
    pub passed: usize,
    /// Number of targets that failed
    pub failed: usize,
    /// Number of targets that did not run because a prerequisite failed
    pub blocked: usize,
    /// Number of targets that were skipped
    pub skipped: usize,
    /// Number of targets that hit cache
    pub cached: usize,
    /// Total duration in milliseconds
    pub duration_ms: u128,
    /// Remote cache counters, present when a GitHub Actions cache ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_summary: Option<crate::cache::CacheSummary>,
}

/// Print execution summary based on output mode
pub fn print_summary(
    results: &[ExecutionResult],
    target: &str,
    mode: OutputMode,
    is_affected: bool,
) {
    if mode == OutputMode::Json {
        // JSON output already printed separately
        return;
    }

    let skipped = results.iter().filter(|r| r.skipped).count();
    let cached = results.iter().filter(|r| r.cached).count();
    let passed = results
        .iter()
        .filter(|r| r.success && !r.skipped && !r.cached)
        .count();
    let failed = results.iter().filter(|r| !r.success && !r.blocked).count();
    let blocked = results.iter().filter(|r| r.blocked).count();
    let total = results.len();
    let total_duration = summary_duration_ms(results);
    let cached_summary = if cached > 0 {
        format!(", {cached} cached")
    } else {
        String::new()
    };
    let blocked_summary = if blocked > 0 {
        format!(", {blocked} blocked")
    } else {
        String::new()
    };

    let context = if is_affected {
        "affected projects"
    } else {
        "projects"
    };

    if mode == OutputMode::Quiet {
        // Single line summary for quiet mode
        let mut parts = vec![format!("{passed} passed{cached_summary}")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if blocked > 0 {
            parts.push(format!("{blocked} blocked"));
        }
        eprintln!("{}", parts.join(", "));
        return;
    }

    // Normal or Verbose mode - detailed summary
    println!("\n=== Summary ===");
    if skipped > 0 {
        println!(
            "Ran '{target}' on {total} {context}: {passed} passed{cached_summary}, {failed} failed{blocked_summary}, {skipped} skipped (no target)"
        );
    } else {
        println!(
            "Ran '{target}' on {total} {context}: {passed} passed{cached_summary}, {failed} failed{blocked_summary}"
        );
    }
    println!("Total time: {total_duration}ms");

    let quarantined: Vec<_> = results
        .iter()
        .filter_map(|r| {
            let tests = &r.tests.as_ref()?.quarantined;
            (r.success && !tests.is_empty()).then_some((r, tests))
        })
        .collect();
    if !quarantined.is_empty() {
        println!("\nPassed with quarantined failures:");
        for (result, tests) in quarantined {
            println!("  - {} (quarantined: {})", result.address, tests.len());
            for test in tests {
                println!("      {} ({})", test.name, test.source);
            }
        }
    }

    if failed > 0 {
        println!("\nFailed projects:");
        for result in results.iter().filter(|r| !r.success && !r.blocked) {
            println!("  - {}", result.address);
        }
    }

    if blocked > 0 {
        println!("\nBlocked projects:");
        for result in results.iter().filter(|r| r.blocked) {
            println!("  - {}", result.address);
        }
    }
}

/// Build ExecutionOutput from execution results
pub fn build_execution_output(results: &[ExecutionResult]) -> ExecutionOutput {
    build_execution_output_with_cache(results, None)
}

/// [`build_execution_output`] plus the remote cache counters from this run.
pub fn build_execution_output_with_cache(
    results: &[ExecutionResult],
    cache_summary: Option<crate::cache::CacheSummary>,
) -> ExecutionOutput {
    let target_results: Vec<TargetResult> = results
        .iter()
        .map(|r| {
            let status = if r.interrupted {
                "interrupted".to_string()
            } else if r.skipped {
                "skipped".to_string()
            } else if r.cached {
                "cached".to_string()
            } else if r.blocked {
                "blocked".to_string()
            } else if r.success {
                "passed".to_string()
            } else {
                "failed".to_string()
            };

            // A cache hit reports the stored exit code. Failures are never
            // cached, so this is 0. Skips never ran a process.
            let exit_code = if r.skipped { None } else { r.exit_code };

            TargetResult {
                address: r.address.clone(),
                status,
                cached: r.cached,
                exit_code,
                duration_ms: r.duration_ms,
                output: if r.output.is_empty() {
                    None
                } else {
                    Some(r.output.clone())
                },
                tests: r.tests.as_ref().map(test_summary),
            }
        })
        .collect();

    let total = results.len();
    let skipped = results.iter().filter(|r| r.skipped).count();
    let cached = results.iter().filter(|r| r.cached).count();
    let passed = results
        .iter()
        .filter(|r| r.success && !r.skipped && !r.cached)
        .count();
    let failed = results.iter().filter(|r| !r.success && !r.blocked).count();
    let blocked = results.iter().filter(|r| r.blocked).count();
    let total_duration = summary_duration_ms(results);

    ExecutionOutput {
        results: target_results,
        summary: ExecutionSummary {
            total,
            passed,
            failed,
            blocked,
            skipped,
            cached,
            duration_ms: total_duration,
            cache_summary,
        },
    }
}

/// Wall-clock time of the run when the executor recorded it, otherwise the
/// sum of per-target durations.
fn summary_duration_ms(results: &[ExecutionResult]) -> u128 {
    results
        .iter()
        .find_map(|result| result.run_wall_ms)
        .unwrap_or_else(|| results.iter().map(|result| result.duration_ms).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_mode_equality() {
        assert_eq!(OutputMode::Json, OutputMode::Json);
        assert_ne!(OutputMode::Json, OutputMode::Normal);
    }

    #[test]
    fn test_project_info_serialization() {
        let mut targets = HashMap::new();
        targets.insert("test".to_string(), "npm test".to_string());

        let info = ProjectInfo {
            address: "//services/api".to_string(),
            path: "services/api".to_string(),
            plugin: "nodejs".to_string(),
            languages: vec!["nodejs".to_string()],
            build_system: None,
            targets,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("//services/api"));
        assert!(json.contains("nodejs"));
    }

    #[test]
    fn test_graph_output_serialization() {
        let mut edges = HashMap::new();
        edges.insert("//a:test".to_string(), vec!["//a:deps".to_string()]);

        let output = GraphOutput {
            nodes: vec!["//a:test".to_string(), "//a:deps".to_string()],
            edges,
            workspace_inputs: HashMap::new(),
            selected_by: HashMap::new(),
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("nodes"));
        assert!(json.contains("edges"));
        assert!(!json.contains("workspace_inputs"));
        assert!(!json.contains("selected_by"));
    }

    #[test]
    fn test_why_output_serialization() {
        let output = WhyOutput {
            from: "//a".to_string(),
            to: "//b".to_string(),
            path: Some(vec![
                "//a".to_string(),
                "//c".to_string(),
                "//b".to_string(),
            ]),
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("from"));
        assert!(json.contains("path"));
    }

    #[test]
    fn test_why_output_no_path() {
        let output = WhyOutput {
            from: "//a".to_string(),
            to: "//b".to_string(),
            path: None,
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("null"));
    }

    #[test]
    fn test_execution_summary_serialization() {
        let summary = ExecutionSummary {
            total: 10,
            passed: 8,
            failed: 1,
            blocked: 0,
            skipped: 1,
            cached: 0,
            duration_ms: 5000,
            cache_summary: None,
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"total\":10"));
        assert!(json.contains("\"passed\":8"));
    }

    #[test]
    fn test_build_execution_output() {
        let results = vec![
            ExecutionResult {
                address: "//a:test".to_string(),
                success: true,
                skipped: false,
                cached: false,
                output: "test output".to_string(),
                log: b"test output".to_vec(),
                duration_ms: 100,
                exit_code: Some(0),
                tests: None,
                input_hash: None,
                interrupted: false,
                blocked: false,
                run_wall_ms: None,
            },
            ExecutionResult {
                address: "//b:test".to_string(),
                success: false,
                skipped: false,
                cached: false,
                output: "error".to_string(),
                log: b"error".to_vec(),
                duration_ms: 50,
                exit_code: Some(3),
                tests: None,
                input_hash: None,
                interrupted: false,
                blocked: false,
                run_wall_ms: None,
            },
            ExecutionResult {
                address: "//c:test".to_string(),
                success: true,
                skipped: true,
                cached: false,
                output: "".to_string(),
                log: Vec::new(),
                duration_ms: 0,
                exit_code: None,
                tests: None,
                input_hash: None,
                interrupted: false,
                blocked: false,
                run_wall_ms: None,
            },
        ];

        let output = build_execution_output(&results);

        assert_eq!(output.summary.total, 3);
        assert_eq!(output.summary.passed, 1);
        assert_eq!(output.summary.failed, 1);
        assert_eq!(output.summary.blocked, 0);
        assert_eq!(output.summary.skipped, 1);
        assert_eq!(output.summary.duration_ms, 150);

        assert_eq!(output.results[0].status, "passed");
        assert_eq!(output.results[0].exit_code, Some(0));
        assert_eq!(output.results[1].status, "failed");
        assert_eq!(output.results[1].exit_code, Some(3));
        assert_eq!(output.results[2].status, "skipped");
        assert_eq!(output.results[2].exit_code, None);
        let json = serde_json::to_value(&output).unwrap();
        assert!(json["results"][0].get("tests").is_none());
    }

    #[test]
    fn blocked_targets_are_not_failures_and_wall_clock_replaces_the_sum() {
        let mut blocked = ExecutionResult {
            address: "//after:build".to_string(),
            success: false,
            skipped: false,
            cached: false,
            output: "Blocked: prerequisite target(s) failed: //bad:build".to_string(),
            log: Vec::new(),
            duration_ms: 0,
            exit_code: None,
            tests: None,
            input_hash: None,
            interrupted: false,
            blocked: true,
            run_wall_ms: Some(30),
        };
        let failed = ExecutionResult {
            address: "//bad:build".to_string(),
            success: false,
            skipped: false,
            cached: false,
            output: String::new(),
            log: Vec::new(),
            duration_ms: 100,
            exit_code: Some(9),
            tests: None,
            input_hash: None,
            interrupted: false,
            blocked: false,
            run_wall_ms: Some(30),
        };
        let output = build_execution_output(&[failed, blocked.clone()]);
        assert_eq!(output.summary.failed, 1);
        assert_eq!(output.summary.blocked, 1);
        assert_eq!(output.summary.duration_ms, 30);
        assert_eq!(output.results[1].status, "blocked");

        blocked.run_wall_ms = None;
        blocked.duration_ms = 40;
        let untimed = build_execution_output(&[blocked]);
        assert_eq!(untimed.summary.duration_ms, 40);
        assert_eq!(untimed.summary.failed, 0);
    }

    #[test]
    fn json_output_summarizes_recorded_tests() {
        use crate::test_results::{Confidence, TestCase, TestOutcome, TestReport};
        let case = |name: &str, outcome| TestCase {
            file: Some("t.py".to_string()),
            suite: Some("tests.t".to_string()),
            name: name.to_string(),
            outcome,
            attempt: 1,
            duration_ms: None,
            message: Some("boom".to_string()),
        };
        let result = ExecutionResult {
            address: "//a:test".to_string(),
            success: false,
            skipped: false,
            cached: false,
            output: String::new(),
            log: Vec::new(),
            duration_ms: 1,
            exit_code: Some(1),
            tests: Some(TestReport {
                project: None,
                capture: "junit".to_string(),
                confidence: Confidence::Partial,
                cases: vec![
                    case("a", TestOutcome::Pass),
                    case("b", TestOutcome::Fail),
                    case("c", TestOutcome::Error),
                    case("d", TestOutcome::Skip),
                ],
                quarantined: Vec::new(),
            }),
            input_hash: None,
            interrupted: false,
            blocked: false,
            run_wall_ms: None,
        };
        let json = serde_json::to_value(build_execution_output(&[result])).unwrap();
        let tests = &json["results"][0]["tests"];
        assert_eq!(tests["capture"], "junit");
        assert_eq!(tests["confidence"], "partial");
        assert_eq!(tests["total"], 4);
        assert_eq!(tests["passed"], 1);
        assert_eq!(tests["failed"], 2);
        assert_eq!(tests["skipped"], 1);
        assert_eq!(tests["failures"][0]["name"], "b");
        assert_eq!(tests["failures"][1]["suite"], "tests.t");
        assert!(tests["failures"][0].get("message").is_none());
    }
}
