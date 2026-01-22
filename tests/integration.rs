//! End-to-end integration tests for the aster CLI
//!
//! These tests create temporary workspaces with various project configurations
//! and verify the CLI commands work correctly.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

/// Set up a minimal workspace with .git marker
fn setup_workspace(tmp: &TempDir) {
    fs::create_dir(tmp.path().join(".git")).unwrap();
}

/// Write a package.json file at the given path
fn write_package_json(tmp: &TempDir, path: &str, content: &str) {
    let full_path = tmp.path().join(path);
    fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    fs::write(full_path, content).unwrap();
}

/// Write an aster.toml file at the given path
fn write_aster_toml(tmp: &TempDir, path: &str, content: &str) {
    let full_path = tmp.path().join(path);
    fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    fs::write(full_path, content).unwrap();
}

#[test]
fn test_list_shows_projects() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("list")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//services/api"), "Expected //services/api in output: {}", stdout);
    assert!(stdout.contains("//libs/core"), "Expected //libs/core in output: {}", stdout);
}

#[test]
fn test_list_empty_workspace() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("list")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.is_empty() || stdout.trim().is_empty());
}

#[test]
fn test_graph_shows_all_projects() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//libs/core"));
    assert!(stdout.contains("//services/api"));
}

#[test]
fn test_graph_shows_dependencies() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//services/api"));
    assert!(stdout.contains("-> //libs/core"), "Expected dependency arrow in output: {}", stdout);
}

#[test]
fn test_graph_specific_project() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["graph", "//services/api"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//services/api"));
    assert!(stdout.contains("-> //libs/core"));
}

#[test]
fn test_graph_project_not_found() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["graph", "//nonexistent"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found") || stderr.contains("Project not found"));
}

#[test]
fn test_cycle_detection_fails() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create circular dependency via aster.toml
    write_package_json(&tmp, "a/package.json", r#"{"name": "a"}"#);
    write_aster_toml(&tmp, "a/aster.toml", r#"depends_on = ["//b"]"#);

    write_package_json(&tmp, "b/package.json", r#"{"name": "b"}"#);
    write_aster_toml(&tmp, "b/aster.toml", r#"depends_on = ["//a"]"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(!output.status.success(), "Expected command to fail on cycle");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("cycle"),
        "Expected 'cycle' in error message: {}",
        stderr
    );
}

#[test]
fn test_cycle_shows_path() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create a longer cycle: a -> b -> c -> a
    write_package_json(&tmp, "a/package.json", r#"{"name": "a"}"#);
    write_aster_toml(&tmp, "a/aster.toml", r#"depends_on = ["//b"]"#);

    write_package_json(&tmp, "b/package.json", r#"{"name": "b"}"#);
    write_aster_toml(&tmp, "b/aster.toml", r#"depends_on = ["//c"]"#);

    write_package_json(&tmp, "c/package.json", r#"{"name": "c"}"#);
    write_aster_toml(&tmp, "c/aster.toml", r#"depends_on = ["//a"]"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Check that the cycle path is shown with arrows
    assert!(
        stderr.contains("->"),
        "Expected cycle path with arrows in error: {}",
        stderr
    );
}

#[test]
fn test_verbose_output() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--verbose", "list"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Workspace root:"));
    assert!(stderr.contains("Discovered"));
}

#[test]
fn test_not_in_workspace() {
    let tmp = TempDir::new().unwrap();
    // No .git or aster.toml, so not a workspace

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("list")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("workspace") || stderr.contains("aster.toml") || stderr.contains(".git"),
        "Expected workspace error message: {}",
        stderr
    );
}
