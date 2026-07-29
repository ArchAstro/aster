//! JSON output validation tests
//!
//! Tests that all commands with --json flag produce valid, parseable JSON output.

use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

use super::{setup_workspace, write_file, write_package_json};

/// Helper to create a workspace with multiple Node.js projects
fn setup_workspace_with_projects() -> TempDir {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create two projects: core and api (api depends on core)
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}}"#,
    );

    tmp
}

#[test]
fn test_list_json_outputs_valid_json() {
    let tmp = setup_workspace_with_projects();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "list"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    // Verify stdout is valid JSON array
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("Should be valid JSON array");

    // Should have at least 2 projects
    assert!(
        parsed.len() >= 2,
        "Expected at least 2 projects, got {}",
        parsed.len()
    );

    // Verify structure of each project
    for project in &parsed {
        assert!(project.get("address").is_some(), "Missing 'address' field");
        assert!(project.get("path").is_some(), "Missing 'path' field");
        assert!(project.get("plugin").is_some(), "Missing 'plugin' field");
        assert!(project.get("targets").is_some(), "Missing 'targets' field");
    }
}

#[test]
fn test_list_json_empty_workspace() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "list"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("Should be valid JSON array");

    assert!(
        parsed.is_empty(),
        "Expected empty array for empty workspace"
    );
}

#[test]
fn test_graph_json_outputs_valid_json() {
    let tmp = setup_workspace_with_projects();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "graph"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");

    // Verify structure has "nodes" and "edges" keys
    assert!(parsed.get("nodes").is_some(), "Missing 'nodes' key");
    assert!(parsed.get("edges").is_some(), "Missing 'edges' key");

    // Nodes should be an array
    let nodes = parsed.get("nodes").unwrap();
    assert!(nodes.is_array(), "nodes should be an array");

    // Edges should be an object (map of address -> dependencies)
    let edges = parsed.get("edges").unwrap();
    assert!(edges.is_object(), "edges should be an object");
}

#[test]
fn test_why_json_outputs_valid_json() {
    let tmp = setup_workspace_with_projects();

    // Use target addresses (with :target suffix)
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "why", "//services/api:deps", "//libs/core:deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");

    // Verify structure has "from", "to", "path" keys
    assert!(parsed.get("from").is_some(), "Missing 'from' key");
    assert!(parsed.get("to").is_some(), "Missing 'to' key");
    assert!(parsed.get("path").is_some(), "Missing 'path' key");
}

#[test]
fn test_why_json_no_path_found() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Two unrelated projects
    write_package_json(&tmp, "a/package.json", r#"{"name": "a"}"#);
    write_package_json(&tmp, "b/package.json", r#"{"name": "b"}"#);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "why", "//a:deps", "//b:deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");

    // path should be null when no path exists
    assert_eq!(
        parsed.get("path").unwrap(),
        &Value::Null,
        "path should be null when no path exists"
    );
}

#[test]
fn test_logs_json_no_previous_run() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "logs"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output empty object for no previous run
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON");
    assert!(
        parsed.is_object() || parsed.is_null(),
        "Should be object or null for no previous run"
    );
}

#[test]
fn test_logs_json_specific_target_not_found() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "logs", "//nonexistent:target"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command should succeed even for missing target"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output empty object for not found target
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON");
    assert!(parsed.is_object(), "Should be object for missing target");
}

#[test]
fn test_json_flag_before_subcommand() {
    let tmp = setup_workspace_with_projects();

    // --json must come before the subcommand (global flag)
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    cmd.current_dir(tmp.path())
        .args(["--json", "list"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("["));
}

#[test]
fn test_json_flag_produces_valid_json_for_all_commands() {
    let tmp = setup_workspace_with_projects();

    // Test all commands that support JSON output
    let test_cases = vec![
        vec!["--json", "list"],
        vec!["--json", "graph"],
        vec!["--json", "logs"],
    ];

    for args in test_cases {
        let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
        let output = cmd.current_dir(tmp.path()).args(&args).output().unwrap();

        assert!(
            output.status.success(),
            "Command {args:?} failed: {output:?}"
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let _: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("Command {args:?} did not produce valid JSON: {e}"));
    }
}

/// A pnpm workspace (members in pnpm-workspace.yaml) where the consumer depends on
/// the dependency via the `workspace:*` protocol. aster must infer a build edge so
/// the dependency builds before the consumer.
#[test]
fn test_pnpm_workspace_protocol_build_edge() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // pnpm workspace root: members declared in pnpm-workspace.yaml, not package.json.
    write_file(&tmp, "pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n");
    write_file(&tmp, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");

    write_package_json(
        &tmp,
        "packages/dep/package.json",
        r#"{"name": "@scope/dep", "scripts": {"build": "tsc"}}"#,
    );
    write_package_json(
        &tmp,
        "packages/consumer/package.json",
        r#"{"name": "@scope/consumer", "scripts": {"build": "tsc"},
            "dependencies": {"@scope/dep": "workspace:*"}}"#,
    );

    // graph --json exposes target-level edges (target -> its dependencies).
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "graph"])
        .output()
        .unwrap();
    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");
    let edges = parsed.get("edges").unwrap().as_object().unwrap();

    let consumer_build = edges
        .get("//packages/consumer:build")
        .and_then(|v| v.as_array())
        .expect("consumer:build node should exist");
    assert!(
        consumer_build.contains(&Value::String("//packages/dep:build".to_string())),
        "consumer:build should depend on //packages/dep:build; got {consumer_build:?}"
    );

    // why confirms there is a dependency path from the consumer's build to the dep's.
    let mut why = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let why_out = why
        .current_dir(tmp.path())
        .args([
            "--json",
            "why",
            "//packages/consumer:build",
            "//packages/dep:build",
        ])
        .output()
        .unwrap();
    assert!(why_out.status.success(), "why failed: {why_out:?}");
    let why_json: Value = serde_json::from_str(&String::from_utf8_lossy(&why_out.stdout)).unwrap();
    assert!(
        why_json.get("path").map(|p| !p.is_null()).unwrap_or(false),
        "expected a dependency path consumer:build -> dep:build; got {why_json}"
    );
}
