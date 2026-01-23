//! Real monorepo integration tests
//!
//! These tests run against ~/archastro/firstlanding-wt9, a real polyglot monorepo.
//! They are marked with #[ignore] to prevent CI failures when the monorepo isn't present.
//!
//! To run these tests:
//! ```bash
//! cargo test --ignored
//! ```
//!
//! Requirements:
//! - ~/archastro/firstlanding-wt9 must exist
//! - Must contain an aster.toml or .git file at the root
//!
//! These tests are READ-ONLY and will not modify the monorepo.

use assert_cmd::Command;
use serde_json::Value;
use std::path::PathBuf;

/// Get the path to the real monorepo, or None if it doesn't exist
fn get_monorepo_path() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join("archastro/firstlanding-wt9");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Skip test with message if monorepo is not found
macro_rules! require_monorepo {
    () => {
        match get_monorepo_path() {
            Some(p) => p,
            None => {
                eprintln!("Skipping: monorepo not found at ~/archastro/firstlanding-wt9");
                return;
            }
        }
    };
}

#[test]
#[ignore]
fn test_real_monorepo_discovery() {
    let monorepo_path = require_monorepo!();

    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .arg("list")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.is_empty(),
        "Should discover at least one project in monorepo"
    );

    // Count discovered projects
    let project_count = stdout.lines().filter(|l| l.starts_with("//")).count();
    eprintln!("Discovered {} projects in monorepo", project_count);
    assert!(project_count > 0, "Should discover at least one project");
}

#[test]
#[ignore]
fn test_real_monorepo_list_json() {
    let monorepo_path = require_monorepo!();

    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .args(["--json", "list"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("Should produce valid JSON array");

    eprintln!("Discovered {} projects via JSON", parsed.len());
    assert!(!parsed.is_empty(), "Should discover at least one project");

    // Verify each project has required fields
    for project in &parsed {
        assert!(
            project.get("address").is_some(),
            "Project missing 'address': {:?}",
            project
        );
        assert!(
            project.get("path").is_some(),
            "Project missing 'path': {:?}",
            project
        );
        assert!(
            project.get("plugin").is_some(),
            "Project missing 'plugin': {:?}",
            project
        );
        assert!(
            project.get("targets").is_some(),
            "Project missing 'targets': {:?}",
            project
        );
    }
}

#[test]
#[ignore]
fn test_real_monorepo_graph_completes() {
    let monorepo_path = require_monorepo!();

    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .arg("graph")
        .output()
        .unwrap();

    // Graph should complete without cycle errors
    assert!(
        output.status.success(),
        "Graph command failed (possible cycle?): {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.to_lowercase().contains("cycle"),
        "Graph detected a cycle: {}",
        stderr
    );
}

#[test]
#[ignore]
fn test_real_monorepo_graph_json() {
    let monorepo_path = require_monorepo!();

    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .args(["--json", "graph"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should produce valid JSON");

    // Verify structure
    assert!(parsed.get("nodes").is_some(), "Missing 'nodes' key");
    assert!(parsed.get("edges").is_some(), "Missing 'edges' key");

    let nodes = parsed.get("nodes").unwrap().as_array().unwrap();
    let edges = parsed.get("edges").unwrap().as_object().unwrap();

    eprintln!("Graph has {} nodes and {} edges", nodes.len(), edges.len());
    assert!(!nodes.is_empty(), "Should have at least one node");
}

#[test]
#[ignore]
fn test_real_monorepo_affected_read_only() {
    let monorepo_path = require_monorepo!();

    // Use --base=HEAD to check for uncommitted changes only
    // This is read-only and won't execute any targets
    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .args(["affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    // Command should complete (success or "no projects affected")
    // We don't fail on non-success since tests might not pass
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should not fail with git errors
    assert!(
        !stderr.contains("Not in a git repository"),
        "Should be in a git repo: {}",
        stderr
    );

    eprintln!("Affected command output:\n{}", stdout);
    if stdout.contains("No projects affected") {
        eprintln!("No uncommitted changes detected (expected)");
    }
}

#[test]
#[ignore]
fn test_real_monorepo_logs_command() {
    let monorepo_path = require_monorepo!();

    // Test logs command works (even if no previous run)
    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .arg("logs")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Either shows "No previous run found" or a list of targets
    assert!(
        stdout.contains("No previous run found") || stdout.contains("Last run:"),
        "Expected logs output: {}",
        stdout
    );
}

#[test]
#[ignore]
fn test_real_monorepo_logs_json() {
    let monorepo_path = require_monorepo!();

    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .args(["--json", "logs"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let _: Value = serde_json::from_str(&stdout).expect("Should produce valid JSON");
}

#[test]
#[ignore]
fn test_real_monorepo_polyglot_detection() {
    let monorepo_path = require_monorepo!();

    let mut cmd = Command::cargo_bin("aster").unwrap();
    let output = cmd
        .current_dir(&monorepo_path)
        .args(["--json", "list"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).unwrap();

    // Collect plugin types
    let mut plugins: Vec<String> = parsed
        .iter()
        .filter_map(|p| p.get("plugin").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    plugins.sort();
    plugins.dedup();

    eprintln!("Detected plugin types: {:?}", plugins);

    // A polyglot monorepo should have multiple language types
    // (but we don't strictly require it - just informational)
    if plugins.len() > 1 {
        eprintln!("Confirmed polyglot: {} different project types", plugins.len());
    } else {
        eprintln!("Monorepo appears to be single-language: {:?}", plugins);
    }
}
