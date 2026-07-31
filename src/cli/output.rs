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
    /// Exit code (None if skipped or cached)
    pub exit_code: Option<i32>,
    /// Duration in milliseconds
    pub duration_ms: u128,
    /// Output (stdout + stderr)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
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
    /// Number of targets that were skipped
    pub skipped: usize,
    /// Number of targets that hit cache
    pub cached: usize,
    /// Total duration in milliseconds
    pub duration_ms: u128,
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
    let failed = results.iter().filter(|r| !r.success).count();
    let total = results.len();
    let total_duration: u128 = results.iter().map(|r| r.duration_ms).sum();
    let cached_summary = if cached > 0 {
        format!(", {cached} cached")
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
        if failed > 0 {
            eprintln!("{passed} passed{cached_summary}, {failed} failed");
        } else {
            eprintln!("{passed} passed{cached_summary}");
        }
        return;
    }

    // Normal or Verbose mode - detailed summary
    println!("\n=== Summary ===");
    if skipped > 0 {
        println!(
            "Ran '{target}' on {total} {context}: {passed} passed{cached_summary}, {failed} failed, {skipped} skipped (no target)"
        );
    } else {
        println!(
            "Ran '{target}' on {total} {context}: {passed} passed{cached_summary}, {failed} failed"
        );
    }
    println!("Total time: {total_duration}ms");

    if failed > 0 {
        println!("\nFailed projects:");
        for result in results.iter().filter(|r| !r.success) {
            println!("  - {}", result.address);
        }
    }
}

/// Build ExecutionOutput from execution results
pub fn build_execution_output(results: &[ExecutionResult]) -> ExecutionOutput {
    let target_results: Vec<TargetResult> = results
        .iter()
        .map(|r| {
            let status = if r.skipped {
                "skipped".to_string()
            } else if r.cached {
                "cached".to_string()
            } else if r.success {
                "passed".to_string()
            } else {
                "failed".to_string()
            };

            let exit_code = if r.skipped || r.cached {
                None
            } else {
                Some(if r.success { 0 } else { 1 })
            };

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
    let failed = results.iter().filter(|r| !r.success).count();
    let total_duration: u128 = results.iter().map(|r| r.duration_ms).sum();

    ExecutionOutput {
        results: target_results,
        summary: ExecutionSummary {
            total,
            passed,
            failed,
            skipped,
            cached,
            duration_ms: total_duration,
        },
    }
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
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("nodes"));
        assert!(json.contains("edges"));
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
            skipped: 1,
            cached: 0,
            duration_ms: 5000,
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
                duration_ms: 100,
            },
            ExecutionResult {
                address: "//b:test".to_string(),
                success: false,
                skipped: false,
                cached: false,
                output: "error".to_string(),
                duration_ms: 50,
            },
            ExecutionResult {
                address: "//c:test".to_string(),
                success: true,
                skipped: true,
                cached: false,
                output: "".to_string(),
                duration_ms: 0,
            },
        ];

        let output = build_execution_output(&results);

        assert_eq!(output.summary.total, 3);
        assert_eq!(output.summary.passed, 1);
        assert_eq!(output.summary.failed, 1);
        assert_eq!(output.summary.skipped, 1);
        assert_eq!(output.summary.duration_ms, 150);

        assert_eq!(output.results[0].status, "passed");
        assert_eq!(output.results[1].status, "failed");
        assert_eq!(output.results[2].status, "skipped");
    }
}
