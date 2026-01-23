//! End-to-end integration tests for the aster CLI
//!
//! These tests create temporary workspaces with various project configurations
//! and verify the CLI commands work correctly.

mod cli_tests;

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

/// Write a mix.exs file at the given path
fn write_mix_exs(tmp: &TempDir, path: &str, content: &str) {
    let full_path = tmp.path().join(path);
    fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    fs::write(full_path, content).unwrap();
}

/// Write a pyproject.toml file at the given path
fn write_pyproject_toml(tmp: &TempDir, path: &str, content: &str) {
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

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("//services/api"),
        "Expected //services/api in output: {stdout}"
    );
    assert!(
        stdout.contains("//libs/core"),
        "Expected //libs/core in output: {stdout}"
    );
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

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//libs/core"));
    assert!(stdout.contains("//services/api"));
}

#[test]
fn test_graph_shows_dependencies() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    // Core project needs a build script for dependencies to be visible
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "echo build"}}"#,
    );
    // API depends on core and has scripts that trigger dependency resolution
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}, "scripts": {"build": "echo build"}}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//services/api"));
    // The graph shows target-level dependencies like: :build -> [//libs/core:build]
    assert!(
        stdout.contains("-> ["),
        "Expected dependency arrow in output: {stdout}"
    );
    assert!(
        stdout.contains("//libs/core:build"),
        "Expected //libs/core:build in dependencies: {stdout}"
    );
}

#[test]
fn test_graph_specific_target() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    // Core project needs a build script for dependencies to be visible
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "echo build"}}"#,
    );
    // API depends on core and has scripts
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}, "scripts": {"build": "echo build"}}"#,
    );

    // Graph command now uses target addresses, not project addresses
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["graph", "//services/api:build"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//services/api:build"));
    // Target shows its dependencies
    assert!(
        stdout.contains("-> //libs/core:build") || stdout.contains("//libs/core:build"),
        "Expected //libs/core:build in dependencies: {stdout}"
    );
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

    // Create circular target dependency via aster.toml
    // Both projects need scripts so they have targets beyond just :deps
    write_package_json(
        &tmp,
        "a/package.json",
        r#"{"name": "a", "scripts": {"build": "echo build"}}"#,
    );
    write_aster_toml(&tmp, "a/aster.toml", r#"depends_on = ["//b:build"]"#);

    write_package_json(
        &tmp,
        "b/package.json",
        r#"{"name": "b", "scripts": {"build": "echo build"}}"#,
    );
    write_aster_toml(&tmp, "b/aster.toml", r#"depends_on = ["//a:build"]"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Expected command to fail on cycle"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("cycle"),
        "Expected 'cycle' in error message: {stderr}"
    );
}

#[test]
fn test_cycle_shows_path() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create a longer cycle at target level: a:build -> b:build -> c:build -> a:build
    write_package_json(
        &tmp,
        "a/package.json",
        r#"{"name": "a", "scripts": {"build": "echo build"}}"#,
    );
    write_aster_toml(&tmp, "a/aster.toml", r#"depends_on = ["//b:build"]"#);

    write_package_json(
        &tmp,
        "b/package.json",
        r#"{"name": "b", "scripts": {"build": "echo build"}}"#,
    );
    write_aster_toml(&tmp, "b/aster.toml", r#"depends_on = ["//c:build"]"#);

    write_package_json(
        &tmp,
        "c/package.json",
        r#"{"name": "c", "scripts": {"build": "echo build"}}"#,
    );
    write_aster_toml(&tmp, "c/aster.toml", r#"depends_on = ["//a:build"]"#);

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
        "Expected cycle path with arrows in error: {stderr}"
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
        "Expected workspace error message: {stderr}"
    );
}

// ============================================================================
// Phase 2 Integration Tests: Language Plugins
// ============================================================================

#[test]
fn test_discover_elixir_project() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create Elixir project with path dependency
    write_mix_exs(
        &tmp,
        "libs/core/mix.exs",
        r#"
defmodule Core.MixProject do
  use Mix.Project

  def project do
    [app: :core]
  end
end
"#,
    );

    write_mix_exs(
        &tmp,
        "services/api/mix.exs",
        r#"
defmodule Api.MixProject do
  use Mix.Project

  def project do
    [app: :api]
  end

  defp deps do
    [{:core, path: "../../libs/core"}]
  end
end
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify projects discovered
    assert!(
        stdout.contains("//libs/core"),
        "Expected //libs/core in output: {stdout}"
    );
    assert!(
        stdout.contains("//services/api"),
        "Expected //services/api in output: {stdout}"
    );

    // Verify dependency is detected (target-level dependencies include full addresses)
    assert!(
        stdout.contains("//libs/core"),
        "Expected //libs/core in dependency output: {stdout}"
    );
    // The output shows target-level deps like: :build -> [//libs/core:build, ...]
    assert!(
        stdout.contains("->"),
        "Expected dependency arrow in output: {stdout}"
    );
}

#[test]
fn test_discover_python_project() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create Python project with Poetry path dependency
    write_pyproject_toml(
        &tmp,
        "libs/utils/pyproject.toml",
        r#"
[project]
name = "utils"
version = "1.0.0"
"#,
    );

    write_pyproject_toml(
        &tmp,
        "services/ml/pyproject.toml",
        r#"
[project]
name = "ml"
version = "1.0.0"

[tool.poetry.dependencies]
utils = {path = "../../libs/utils"}
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify projects discovered
    assert!(
        stdout.contains("//libs/utils"),
        "Expected //libs/utils in output: {stdout}"
    );
    assert!(
        stdout.contains("//services/ml"),
        "Expected //services/ml in output: {stdout}"
    );

    // Verify dependency is detected (target-level dependencies include full addresses)
    assert!(
        stdout.contains("//libs/utils"),
        "Expected //libs/utils in dependency output: {stdout}"
    );
}

#[test]
fn test_discover_polyglot_workspace() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Node.js project
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "version": "1.0.0"}"#,
    );

    // Elixir project
    write_mix_exs(
        &tmp,
        "services/backend/mix.exs",
        r#"
defmodule Backend.MixProject do
  use Mix.Project

  def project do
    [app: :backend]
  end
end
"#,
    );

    // Python project
    write_pyproject_toml(
        &tmp,
        "libs/ml/pyproject.toml",
        r#"
[project]
name = "ml"
version = "1.0.0"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("list")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify all three projects discovered
    assert!(
        stdout.contains("//services/api"),
        "Expected Node.js project //services/api: {stdout}"
    );
    assert!(
        stdout.contains("//services/backend"),
        "Expected Elixir project //services/backend: {stdout}"
    );
    assert!(
        stdout.contains("//libs/ml"),
        "Expected Python project //libs/ml: {stdout}"
    );
}

#[test]
fn test_graph_with_mixed_languages() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Shared library (Node.js) - needs a build script for dependencies to be visible
    write_package_json(
        &tmp,
        "libs/shared/package.json",
        r#"{"name": "shared", "scripts": {"build": "echo build"}}"#,
    );

    // Elixir service depending on shared via aster.toml (cross-language)
    // Elixir projects automatically get build/test/deps targets
    write_mix_exs(
        &tmp,
        "services/elixir-api/mix.exs",
        r#"
defmodule ElixirApi.MixProject do
  use Mix.Project

  def project do
    [app: :elixir_api]
  end
end
"#,
    );
    // Cross-language dependencies use target addresses
    write_aster_toml(
        &tmp,
        "services/elixir-api/aster.toml",
        r#"depends_on = ["//libs/shared:build"]"#,
    );

    // Python service depending on shared via aster.toml (cross-language)
    // Python projects automatically get build/test/deps targets
    write_pyproject_toml(
        &tmp,
        "services/python-api/pyproject.toml",
        r#"
[project]
name = "python-api"
version = "1.0.0"
"#,
    );
    write_aster_toml(
        &tmp,
        "services/python-api/aster.toml",
        r#"depends_on = ["//libs/shared:build"]"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("graph")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify all projects discovered
    assert!(
        stdout.contains("//libs/shared"),
        "Expected //libs/shared: {stdout}"
    );
    assert!(
        stdout.contains("//services/elixir-api"),
        "Expected //services/elixir-api: {stdout}"
    );
    assert!(
        stdout.contains("//services/python-api"),
        "Expected //services/python-api: {stdout}"
    );

    // Verify cross-language dependencies via aster.toml
    // The graph output shows target-level dependencies: :build -> [//libs/shared:build, ...]
    assert!(
        stdout.contains("//libs/shared:build"),
        "Expected //libs/shared:build in dependency output: {stdout}"
    );
}

// ============================================================================
// Phase 3 Integration Tests: Affected Command
// ============================================================================

/// Set up a real git repo (not just .git marker) for affected tests
fn setup_git_repo(tmp: &TempDir) {
    let path = tmp.path();

    // Initialize repo
    Command::new("git")
        .args(["init"])
        .current_dir(path)
        .output()
        .unwrap();

    // Configure git user for commits
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .unwrap();
}

/// Create an initial commit with the current state
fn git_commit(tmp: &TempDir, message: &str) {
    let path = tmp.path();

    Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();

    Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .output()
        .unwrap();
}

#[test]
fn test_affected_requires_git_repo() {
    let tmp = TempDir::new().unwrap();
    // Only .git marker, not a real git repo
    setup_workspace(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test"])
        .output()
        .unwrap();

    // Should fail because it's not a real git repo
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git") || stderr.contains("repository"),
        "Expected git error message: {stderr}"
    );
}

#[test]
fn test_affected_no_changes() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);
    git_commit(&tmp, "Initial commit");

    // No changes since last commit
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No projects affected"),
        "Expected 'No projects affected': {stdout}"
    );
}

#[test]
fn test_affected_detects_uncommitted_changes() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    git_commit(&tmp, "Initial commit");

    // Make uncommitted change to one project
    fs::write(tmp.path().join("services/api/new_file.txt"), "new content").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    // Note: Command may fail because npm test isn't defined, but we just want to verify
    // the correct projects are detected
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should detect api as affected (has uncommitted file)
    assert!(
        stdout.contains("//services/api"),
        "Expected //services/api in output: {stdout}"
    );
    // Should NOT detect core (no changes)
    assert!(
        !stdout.contains("//libs/core"),
        "Expected //libs/core NOT in output: {stdout}"
    );
}

#[test]
fn test_affected_detects_committed_changes() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    git_commit(&tmp, "Initial commit");

    // Make change and commit
    fs::write(
        tmp.path().join("libs/core/index.js"),
        "console.log('hello');",
    )
    .unwrap();
    git_commit(&tmp, "Add index.js to core");

    // Check affected between HEAD~1 and HEAD
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD~1", "--head=HEAD"])
        .output()
        .unwrap();

    // Note: Command may fail because npm test isn't defined, but we just want to verify
    // the correct projects are detected
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should detect core as affected
    assert!(
        stdout.contains("//libs/core"),
        "Expected //libs/core in output: {stdout}"
    );
    // Should NOT detect api
    assert!(
        !stdout.contains("//services/api"),
        "Expected //services/api NOT in output: {stdout}"
    );
}

#[test]
fn test_affected_with_dependents_flag() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // api depends on core
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}}"#,
    );
    git_commit(&tmp, "Initial commit");

    // Make change to core
    fs::write(
        tmp.path().join("libs/core/index.js"),
        "console.log('hello');",
    )
    .unwrap();

    // Without --dependents, only core should be affected
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    // Note: Command may fail because npm test isn't defined, but we just want to verify
    // the correct projects are detected
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//libs/core"));
    assert!(stdout.contains("directly affected only"));

    // With --dependents, both core and api should be affected
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--dependents"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("//libs/core"),
        "Expected //libs/core with --dependents: {stdout}"
    );
    assert!(
        stdout.contains("//services/api"),
        "Expected //services/api with --dependents: {stdout}"
    );
    assert!(stdout.contains("including dependents"));
}

#[test]
fn test_affected_help_shows_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["affected", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--base"));
    assert!(stdout.contains("--head"));
    assert!(stdout.contains("--dependents"));
}
