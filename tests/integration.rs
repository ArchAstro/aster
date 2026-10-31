//! End-to-end integration tests for the aster CLI
//!
//! These tests create temporary workspaces with various project configurations
//! and verify the CLI commands work correctly.

mod cli_tests;

use std::fs;
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};
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

/// Write an arbitrary fixture file, creating parent directories.
fn write_fixture(tmp: &TempDir, path: &str, content: &str) {
    let full_path = tmp.path().join(path);
    fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    fs::write(full_path, content).unwrap();
}

#[test]
fn dev_target_does_not_conflict_with_services_command() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.dev]
command = "sh -c 'echo ran > dev-target-ran'"
cache = { enabled = false }
"#,
    );

    let status = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["dev", "//app", "--no-deps"])
        .status()
        .unwrap();

    assert!(status.success());
    assert_eq!(
        fs::read_to_string(tmp.path().join("app/dev-target-ran")).unwrap(),
        "ran\n"
    );
}

#[test]
fn unquoted_shell_operator_fails_config_load_before_anything_runs() {
    // Setup: a project whose second step hides behind `&&`. Aster runs
    // commands without a shell, so this would run one `touch` with the
    // arguments `first`, `&&`, `touch` and `second`; a second program of any
    // other name would never run.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.both]
command = "touch first && touch second"
cache = { enabled = false }
"#,
    );

    // Listing and running both load the configuration, so both refuse it and
    // name the target, the file and the operator.
    for args in [&["list"][..], &["run", "//app:both"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success(), "{args:?} succeeded: {stderr}");
        assert!(
            stderr.contains("Target 'both'")
                && stderr.contains("aster.toml")
                && stderr.contains("`&&`")
                && stderr.contains("bash -c"),
            "{args:?}: {stderr}"
        );
    }

    // The refusal happens before any process starts.
    assert!(!tmp.path().join("app/first").exists());
}

#[test]
fn explicit_shell_runs_every_step_of_a_chained_command() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.both]
command = "sh -c 'touch first && touch second'"
cache = { enabled = false }
"#,
    );

    let status = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:both"])
        .status()
        .unwrap();

    assert!(status.success());
    assert!(tmp.path().join("app/first").exists());
    assert!(tmp.path().join("app/second").exists());
}

#[cfg(unix)]
#[test]
fn terminating_aster_cleans_up_the_target_process_group() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.long]
command = "sh long.sh"
cache = { enabled = false }
"#,
    );
    fs::write(
        tmp.path().join("app/long.sh"),
        "#!/bin/sh\necho $$ > child.pid\nexec sleep 30\n",
    )
    .unwrap();

    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:long"])
        .spawn()
        .unwrap();

    let child_pid_path = tmp.path().join("app/child.pid");
    // `echo $$ > child.pid` creates the file before writing the pid. A poll
    // that stops at existence can read that empty file under suite load.
    let deadline = Instant::now() + Duration::from_secs(5);
    let child_pid = loop {
        if let Ok(pid) = fs::read_to_string(&child_pid_path)
            .unwrap_or_default()
            .trim()
            .parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "target process never started");
        std::thread::sleep(Duration::from_millis(20));
    };

    unsafe {
        libc::kill(aster.id() as i32, libc::SIGTERM);
    }
    let status = aster.wait().unwrap();
    assert_eq!(status.code(), Some(143));

    let child_exists = unsafe { libc::kill(child_pid, 0) == 0 };
    assert!(!child_exists, "target process survived Aster termination");
}

#[cfg(unix)]
#[test]
fn terminating_aster_mid_run_launches_nothing_more_and_exits() {
    // `long` is running when SIGTERM arrives. Its dependent `after` is not
    // launched, and Aster drains `long` and exits instead of hanging.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.long]
command = "sh long.sh"
cache = { enabled = false }

[targets.after]
command = "touch after_ran"
depends_on = ["//app:long"]
cache = { enabled = false }
"#,
    );
    fs::write(
        tmp.path().join("app/long.sh"),
        "#!/bin/sh\necho $$ > child.pid\nexec sleep 30\n",
    )
    .unwrap();

    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:after"])
        .spawn()
        .unwrap();

    let child_pid_path = tmp.path().join("app/child.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !child_pid_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child_pid_path.exists(), "target process never started");

    unsafe {
        libc::kill(aster.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match aster.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = aster.kill();
                panic!("aster did not exit after SIGTERM");
            }
        }
    };
    assert_eq!(status.code(), Some(143));
    assert!(
        !tmp.path().join("app/after_ran").exists(),
        "a dependent launched after shutdown"
    );
}

#[cfg(unix)]
#[test]
fn captured_targets_do_not_hang_on_stdin_prompts() {
    use std::io::Read;
    use std::process::Stdio;

    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.prompt]
command = "sh -c 'read -r _ignored; printf ready > marker'"
cache = { enabled = false }
"#,
    );

    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:prompt", "--no-cache"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Keep Aster's stdin write-end open. If the target inherited it, `read`
    // would block until this process closed the pipe — the #72 hang.
    let _stdin = aster.stdin.take();
    let mut stdout = aster.stdout.take().unwrap();
    let mut stderr = aster.stderr.take().unwrap();
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).ok();
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).ok();
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match aster.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = aster.kill();
                let _ = aster.wait();
                panic!("aster hung: captured target inherited stdin and blocked on read");
            }
        }
    };

    let stdout = String::from_utf8_lossy(&stdout_handle.join().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_handle.join().unwrap()).into_owned();
    assert!(
        status.success(),
        "aster failed: status={status:?} stdout={stdout} stderr={stderr}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("app/marker")).unwrap(),
        "ready"
    );
}

#[test]
fn heterogeneous_run_preserves_quoted_arguments() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.write]
command = "sh -c 'printf \"%s\" \"$1\" > quoted.txt' sh 'two words'"
cache = { enabled = false }
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:write"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("app/quoted.txt")).unwrap(),
        "two words"
    );
}

#[test]
fn stale_execution_log_is_absent_while_native_target_runs() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(
        &tmp,
        "app/package.json",
        r#"{"name":"app","scripts":{"test":"test ! -e ../.aster/logs/latest.json"}}"#,
    );
    write_fixture(
        &tmp,
        ".aster/logs/latest.json",
        r#"{"timestamp":"old","target":"test","results":[]}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--no-cache", "test", "//app", "--no-deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    assert!(
        tmp.path().join(".aster/logs/latest.json").is_file(),
        "Aster should recreate the run log after native tools finish"
    );
}

#[test]
fn heterogeneous_run_blocks_dependents_after_failure() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.check]
command = "sh -c 'exit 9'"
cache = { enabled = false }

[targets.deploy]
command = "touch deployed"
depends_on = ["//self:check"]
cache = { enabled = false }
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:deploy"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!tmp.path().join("app/deployed").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Blocked"));
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
        stdout.contains("→ ["),
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

#[test]
fn skills_prints_markdown_without_a_workspace() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .arg("--skills")
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("# Using Aster\n"));
    assert!(stdout.contains("aster lint --all"));
    assert!(stdout.contains("aster test --all"));
    assert!(stdout.contains("aster services up"));
    assert!(stdout.contains("aster services logs api | grep ERROR"));
    assert!(stdout.contains("It does not define policy"));
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
    // The output shows target-level deps like: :build → [//libs/core:build, ...]
    assert!(
        stdout.contains("→"),
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

#[cfg(unix)]
#[test]
fn gradle_module_target_executes_wrapper_from_build_root() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(
        &tmp,
        "settings.gradle.kts",
        r#"rootProject.name = "sample"
include("shared", "app")
"#,
    );
    write_fixture(&tmp, "shared/shared.gradle.kts", "plugins { java }\n");
    write_fixture(
        &tmp,
        "app/app.gradle.kts",
        "plugins { java }\ndependencies { implementation(project(\":shared\")) }\n",
    );
    write_fixture(
        &tmp,
        "gradlew",
        "#!/bin/sh\nprintf '%s\\n' \"$PWD|$*\" >> wrapper-invocations\n",
    );
    let wrapper = tmp.path().join("gradlew");
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--no-cache", "build", "//app", "--no-deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let invocation = fs::read_to_string(tmp.path().join("wrapper-invocations")).unwrap();
    assert!(
        invocation.contains("|:app:build"),
        "expected scoped Gradle task: {invocation}"
    );
    assert!(
        invocation.starts_with(
            &tmp.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_string()
        ),
        "wrapper should run at Gradle root: {invocation}"
    );
}

#[cfg(unix)]
#[test]
fn maven_module_target_executes_wrapper_with_reactor_selection() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(
        &tmp,
        "pom.xml",
        "<project><artifactId>parent</artifactId><packaging>pom</packaging><modules><module>shared</module><module>app</module></modules></project>",
    );
    write_fixture(
        &tmp,
        "shared/pom.xml",
        "<project><artifactId>shared</artifactId></project>",
    );
    write_fixture(
        &tmp,
        "shared/src/main/java/Shared.java",
        "class Shared {}\n",
    );
    write_fixture(
        &tmp,
        "app/pom.xml",
        "<project><artifactId>app</artifactId><dependencies><dependency><groupId>dev.example</groupId><artifactId>shared</artifactId></dependency></dependencies></project>",
    );
    write_fixture(&tmp, "app/src/main/java/App.java", "class App {}\n");
    write_fixture(
        &tmp,
        "mvnw",
        "#!/bin/sh\nprintf '%s\\n' \"$PWD|$*\" >> wrapper-invocations\n",
    );
    let wrapper = tmp.path().join("mvnw");
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--no-cache", "test", "//app", "--no-deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let invocation = fs::read_to_string(tmp.path().join("wrapper-invocations")).unwrap();
    assert!(
        invocation.contains("|-pl :app -am test"),
        "expected Maven reactor selection: {invocation}"
    );
    assert!(
        invocation.starts_with(
            &tmp.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_string()
        ),
        "wrapper should run at Maven reactor root: {invocation}"
    );
}

#[cfg(unix)]
#[test]
fn ruby_member_executes_bundle_install_at_root_and_rspec_in_member() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(&tmp, "Gemfile", "source 'https://rubygems.org'\n");
    write_fixture(
        &tmp,
        "gems/core/core.gemspec",
        "Gem::Specification.new { |s| s.name = 'core' }\n",
    );
    write_fixture(&tmp, "gems/core/.rspec", "--color\n");
    write_fixture(&tmp, "gems/core/spec/core_spec.rb", "# fixture\n");
    write_fixture(
        &tmp,
        "bin/bundle",
        "#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> \"$ASTER_RUBY_LOG\"\n",
    );
    let wrapper = tmp.path().join("bin/bundle");
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let path = format!(
        "{}:{}",
        tmp.path().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let log = tmp.path().join("bundle-invocations");
    for (target, address) in [("deps", "//gems/core"), ("test", "//gems/core")] {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .env("PATH", &path)
            .env("ASTER_RUBY_LOG", &log)
            .args(["--no-cache", target, address, "--no-deps"])
            .output()
            .unwrap();
        assert!(output.status.success(), "Command failed: {output:?}");
    }

    let invocations = fs::read_to_string(log).unwrap();
    let root = tmp.path().canonicalize().unwrap();
    assert!(
        invocations.contains(&format!("{}|install", root.display())),
        "bundle install should run at the nearest Gemfile: {invocations}"
    );
    assert!(
        invocations.contains(&format!("{}|exec rspec", root.join("gems/core").display())),
        "RSpec should run in the selected gem: {invocations}"
    );
}

#[test]
fn ruby_gem_takes_precedence_over_colocated_javascript_assets() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(&tmp, "app/Gemfile", "gemspec\n");
    write_fixture(
        &tmp,
        "app/app.gemspec",
        "Gem::Specification.new { |s| s.name = 'app' }\n",
    );
    write_package_json(
        &tmp,
        "app/package.json",
        r#"{"name":"app-assets","scripts":{"build":"vite build"}}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "ruby"])
        .output()
        .unwrap();
    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//app"));

    let node = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "nodejs"])
        .output()
        .unwrap();
    assert!(node.status.success(), "Command failed: {node:?}");
    assert!(!String::from_utf8_lossy(&node.stdout).contains("//app"));
}

#[test]
fn java_and_kotlin_filters_are_independent_from_jvm_build_systems() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(&tmp, "gradle-java/build.gradle", "apply plugin: 'java'\n");
    write_fixture(
        &tmp,
        "gradle-kotlin/build.gradle.kts",
        "plugins { kotlin(\"jvm\") version \"2.1.0\" }\n",
    );
    write_fixture(
        &tmp,
        "gradle-mixed/build.gradle.kts",
        "plugins { kotlin(\"jvm\") version \"2.1.0\" }\n",
    );
    write_fixture(
        &tmp,
        "gradle-mixed/src/main/java/App.java",
        "class App {}\n",
    );
    write_fixture(&tmp, "gradle-mixed/src/main/kotlin/App.kt", "class App\n");
    write_fixture(
        &tmp,
        "maven-java/pom.xml",
        "<project><artifactId>maven-java</artifactId></project>",
    );
    write_fixture(&tmp, "maven-java/src/main/java/App.java", "class App {}\n");
    write_fixture(
        &tmp,
        "maven-kotlin/pom.xml",
        "<project><artifactId>maven-kotlin</artifactId><build><plugins><plugin><artifactId>kotlin-maven-plugin</artifactId></plugin></plugins></build></project>",
    );
    write_fixture(&tmp, "maven-kotlin/src/main/kotlin/App.kt", "class App\n");

    let java = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "java"])
        .output()
        .unwrap();
    assert!(java.status.success(), "Command failed: {java:?}");
    let stdout = String::from_utf8_lossy(&java.stdout);
    assert!(stdout.contains("//gradle-java"));
    assert!(stdout.contains("//maven-java"));
    assert!(stdout.contains("//gradle-mixed"));
    assert!(!stdout.contains("//gradle-kotlin"));
    assert!(!stdout.contains("//maven-kotlin"));

    let kotlin = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "kotlin"])
        .output()
        .unwrap();
    assert!(kotlin.status.success(), "Command failed: {kotlin:?}");
    let stdout = String::from_utf8_lossy(&kotlin.stdout);
    assert!(stdout.contains("//gradle-kotlin"));
    assert!(stdout.contains("//maven-kotlin"));
    assert!(stdout.contains("//gradle-mixed"));
    assert!(!stdout.contains("//gradle-java"));
    assert!(!stdout.contains("//maven-java"));
}

#[test]
fn gradle_kotlin_dsl_does_not_imply_kotlin_source() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(&tmp, "app/build.gradle.kts", "plugins { java }\n");

    let java = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "java"])
        .output()
        .unwrap();
    assert!(java.status.success(), "Command failed: {java:?}");
    assert!(String::from_utf8_lossy(&java.stdout).contains("//app"));

    let kotlin = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "kotlin"])
        .output()
        .unwrap();
    assert!(kotlin.status.success(), "Command failed: {kotlin:?}");
    assert!(!String::from_utf8_lossy(&kotlin.stdout).contains("//app"));
}

#[test]
fn list_json_reports_languages_and_build_system_separately() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(&tmp, "app/build.gradle", "apply plugin: 'java'\n");

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--json", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "Command failed: {output:?}");
    let projects: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(projects[0]["languages"], serde_json::json!(["java"]));
    assert_eq!(projects[0]["build_system"], "gradle");
    assert_eq!(projects[0]["plugin"], "gradle");
}

#[test]
fn discovery_skips_git_and_node_modules_without_gitignore() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    assert!(
        !tmp.path().join(".gitignore").exists(),
        "fixture must not rely on .gitignore"
    );

    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        "[targets]\nbuild = \"sh -c 'echo built'\"\n",
    );
    write_package_json(
        &tmp,
        "app/node_modules/is-odd/package.json",
        r#"{"name":"is-odd"}"#,
    );
    write_package_json(&tmp, ".git/x/package.json", r#"{"name":"git-x"}"#);

    // Keep a machine-wide gitignore from hiding the fixture and making the
    // hard skip look true for the wrong reason.
    let isolated = tmp.path().join("isolated-home");
    fs::create_dir_all(isolated.join(".config")).unwrap();
    let aster = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .env("HOME", &isolated)
            .env("XDG_CONFIG_HOME", isolated.join(".config"))
            .env("GIT_CONFIG_GLOBAL", isolated.join("no-gitconfig"))
            .env("GIT_CONFIG_SYSTEM", isolated.join("no-gitconfig"))
            .args(args)
            .output()
            .unwrap()
    };

    let list = aster(&["list", "--json"]);
    assert!(
        list.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let projects: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let addresses: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|project| project["address"].as_str().unwrap())
        .collect();
    assert_eq!(addresses, vec!["//app"], "{projects}");

    let build = aster(&["build", "--all", "--json"]);
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&build.stdout).unwrap();
    let ran: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["address"].as_str().unwrap())
        .collect();
    assert_eq!(ran, vec!["//app:build"], "{body}");
    assert_eq!(body["results"][0]["status"], "passed");
}

#[test]
fn gradle_and_maven_are_not_language_filters() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(&tmp, "app/build.gradle", "apply plugin: 'java'\n");

    for build_system in ["gradle", "maven"] {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(["list", "--lang", build_system])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("Unknown language '{build_system}'")));
        assert!(stderr.contains("java"));
        assert!(stderr.contains("kotlin"));
    }
}

#[test]
fn test_python_poetry_wraps_commands() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create Poetry Python project with tools configured
    write_pyproject_toml(
        &tmp,
        "services/api/pyproject.toml",
        r#"
[tool.poetry]
name = "api"
version = "1.0.0"

[tool.pytest.ini_options]
testpaths = ["tests"]

[tool.ruff]
line-length = 100
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Poetry projects should wrap commands with "poetry run"
    assert!(
        stdout.contains("poetry run pytest"),
        "Expected 'poetry run pytest' for test command: {stdout}"
    );
    assert!(
        stdout.contains("poetry run ruff"),
        "Expected 'poetry run ruff' for lint/format commands: {stdout}"
    );
    assert!(
        stdout.contains("poetry install"),
        "Expected 'poetry install' for deps command: {stdout}"
    );
}

#[test]
fn test_python_uv_wraps_commands() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create uv Python project (has uv.lock file)
    write_pyproject_toml(
        &tmp,
        "services/api/pyproject.toml",
        r#"
[project]
name = "api"
version = "1.0.0"

[tool.pytest.ini_options]
testpaths = ["tests"]

[tool.ruff]
line-length = 100
"#,
    );

    // Create uv.lock to indicate uv project
    fs::write(tmp.path().join("services/api/uv.lock"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // uv projects should wrap commands with "uv run"
    assert!(
        stdout.contains("uv run pytest"),
        "Expected 'uv run pytest' for test command: {stdout}"
    );
    assert!(
        stdout.contains("uv run ruff"),
        "Expected 'uv run ruff' for lint/format commands: {stdout}"
    );
    assert!(
        stdout.contains("uv sync"),
        "Expected 'uv sync' for deps command: {stdout}"
    );
}

#[test]
fn test_nodejs_skips_npm_placeholder_test() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create Node.js project with npm's default placeholder test script
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "scripts": {"build": "echo build", "test": "echo \"Error: no test specified\" && exit 1"}}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should have deps target (always present)
    assert!(
        stdout.contains("\"deps\""),
        "Expected deps target in output: {stdout}"
    );
    // Should have build target (explicitly defined)
    assert!(
        stdout.contains("\"build\""),
        "Expected build target in output: {stdout}"
    );
    // Should NOT have test target (placeholder should be skipped)
    assert!(
        !stdout.contains("\"test\""),
        "Expected NO test target for npm placeholder script: {stdout}"
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
fn test_affected_ignore_prevents_root_project_from_claiming_ignored_changes() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(
        &tmp,
        "package.json",
        r#"{"name": "workspace", "scripts": {"test": "true"}}"#,
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        r#"
[affected]
ignore = [".agents/**/*.md"]
"#,
    );
    git_commit(&tmp, "Initial commit");

    fs::create_dir_all(tmp.path().join(".agents/skills/example")).unwrap();
    fs::write(
        tmp.path().join(".agents/skills/example/SKILL.md"),
        "new untracked skill",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No projects affected"),
        "Root project should not claim ignored paths: {stdout}"
    );
}

#[test]
fn test_affected_ignore_filters_verbose_and_dry_run_file_lists() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(
        &tmp,
        "package.json",
        r#"{"name": "workspace", "scripts": {"test": "true"}}"#,
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        r#"
[affected]
ignore = [".agents/**"]

[targets.test]
command = "./capture-files.sh {files}"
capabilities = ["files_list"]
"#,
    );
    fs::write(
        tmp.path().join("capture-files.sh"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > affected-files.txt\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(tmp.path().join("capture-files.sh"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(tmp.path().join("capture-files.sh"), permissions).unwrap();
    }
    fs::create_dir_all(tmp.path().join(".agents/skills/example")).unwrap();
    fs::write(
        tmp.path().join(".agents/skills/example/SKILL.md"),
        "original",
    )
    .unwrap();
    git_commit(&tmp, "Initial commit");

    fs::write(
        tmp.path().join(".agents/skills/example/SKILL.md"),
        "updated",
    )
    .unwrap();
    fs::write(
        tmp.path().join("package.json"),
        r#"{"name": "workspace", "scripts": {"test": "true"}, "private": true}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "--verbose",
            "affected",
            "test",
            "--base=HEAD",
            "--dry-run",
            "--only-affected-files",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("//:test"),
        "Root package should remain affected: {stdout}"
    );
    assert!(
        stdout.contains("package.json"),
        "Expected real changed file: {stdout}"
    );
    assert!(
        !stdout.contains("SKILL.md") && !stderr.contains("SKILL.md"),
        "Ignored file leaked into affected output: stdout={stdout}, stderr={stderr}"
    );
    assert!(
        stderr.contains("Found 1 changed files")
            && stderr.contains("Ignored 1 changed files via affected.ignore"),
        "Expected verbose counts after filtering: {stderr}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--base=HEAD",
            "--only-affected-files",
            "--no-cache",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let affected_files =
        fs::read_to_string(tmp.path().join("affected-files.txt")).unwrap_or_else(|error| {
            panic!(
                "files-list command did not write output: {error}; stdout={}; stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert_eq!(affected_files.trim(), "package.json");
}

#[test]
fn test_affected_ignore_rejects_invalid_glob() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(&tmp, "package.json", r#"{"name": "workspace"}"#);
    write_aster_toml(
        &tmp,
        "aster.toml",
        r#"
[affected]
ignore = ["["]
"#,
    );
    git_commit(&tmp, "Initial commit");

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid affected.ignore pattern: ["),
        "Expected invalid pattern context: {stderr}"
    );
}

#[test]
fn test_affected_ignore_filters_rename_paths_independently() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(
        &tmp,
        "package.json",
        r#"{"name": "workspace", "scripts": {"test": "true"}}"#,
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        r#"
[affected]
ignore = [".agents/**"]
"#,
    );
    fs::write(tmp.path().join("runtime.js"), "content").unwrap();
    git_commit(&tmp, "Initial commit");

    fs::create_dir_all(tmp.path().join(".agents/archive")).unwrap();
    let status = Command::new("git")
        .args(["mv", "runtime.js", ".agents/archive/runtime.js"])
        .current_dir(tmp.path())
        .status()
        .unwrap();
    assert!(status.success());
    git_commit(&tmp, "Move runtime file into ignored metadata");

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--base=HEAD~1",
            "--head=HEAD",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("runtime.js"),
        "The non-ignored old rename path should remain affected: {stdout}"
    );
    assert!(
        !stdout.contains(".agents/archive/runtime.js"),
        "The ignored new rename path should be filtered: {stdout}"
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
fn test_affected_lanes_split_elixir_primary_projects_and_keep_target_dependencies() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    let mix_project = |module: &str, app: &str| {
        format!(
            "defmodule {module}.MixProject do\n  use Mix.Project\n  def project, do: [app: :{app}, version: \"0.1.0\"]\nend\n"
        )
    };
    write_mix_exs(
        &tmp,
        "src/elixir/core/accounts/mix.exs",
        &mix_project("Accounts", "accounts"),
    );
    write_mix_exs(
        &tmp,
        "services/platform/gateway/mix.exs",
        &mix_project("Gateway", "gateway"),
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        r#"
[affected.lanes.core]
include = ["//src/elixir/core/..."]

[affected.lanes.platform]
include = ["//services/platform/..."]

[affected.lanes.with_exclusion]
exclude = ["//services/platform/..."]
"#,
    );
    write_aster_toml(
        &tmp,
        "src/elixir/core/accounts/aster.toml",
        r#"
[targets.build]
command = "true"
depends_on = ["//services/platform/gateway:prepare"]
"#,
    );
    write_aster_toml(
        &tmp,
        "services/platform/gateway/aster.toml",
        r#"
[targets.build]
command = "true"

[targets.prepare]
command = "true"
"#,
    );
    git_commit(&tmp, "Initial commit");
    write_fixture(&tmp, "src/elixir/core/accounts/lib/change.ex", "changed");
    write_fixture(&tmp, "services/platform/gateway/lib/change.ex", "changed");

    let run_lane = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args([
                "affected",
                "build",
                "--base=HEAD",
                "--dry-run",
                "--json",
                "--lane",
                name,
            ])
            .output()
            .unwrap()
    };

    let core = run_lane("core");
    assert!(core.status.success(), "core lane failed: {core:?}");
    let core_json: serde_json::Value = serde_json::from_slice(&core.stdout).unwrap();
    let core_targets = core_json["targets"].as_array().unwrap();
    assert!(core_targets.iter().any(|target| {
        target["address"] == "//src/elixir/core/accounts:build" && target["reason"] == "affected"
    }));
    assert!(core_targets.iter().any(|target| {
        target["address"] == "//services/platform/gateway:prepare"
            && target["reason"] == "target dependency"
    }));
    assert!(!core_targets
        .iter()
        .any(|target| target["address"] == "//services/platform/gateway:build"));

    let platform = run_lane("platform");
    assert!(
        platform.status.success(),
        "platform lane failed: {platform:?}"
    );
    let platform_stdout = String::from_utf8_lossy(&platform.stdout);
    assert!(platform_stdout.contains("//services/platform/gateway:build"));
    assert!(!platform_stdout.contains("//src/elixir/core/accounts:build"));

    let excluded = run_lane("with_exclusion");
    assert!(
        excluded.status.success(),
        "excluded lane failed: {excluded:?}"
    );
    let excluded_stdout = String::from_utf8_lossy(&excluded.stdout);
    assert!(excluded_stdout.contains("//src/elixir/core/accounts:build"));
    assert!(!excluded_stdout.contains("//services/platform/gateway:build"));

    // Planned together, each lane gets the plan it gets alone.
    let together = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "build", "--base=HEAD", "--dry-run", "--json"])
        .args(["--lane", "core", "--lane", "platform"])
        .output()
        .unwrap();
    assert!(together.status.success(), "lanes failed: {together:?}");
    let together_json: serde_json::Value = serde_json::from_slice(&together.stdout).unwrap();
    let platform_json: serde_json::Value = serde_json::from_slice(&platform.stdout).unwrap();
    assert_eq!(together_json["lanes"]["core"], core_json);
    assert_eq!(together_json["lanes"]["platform"], platform_json);
    assert_eq!(together_json["lanes"].as_object().unwrap().len(), 2);

    let human = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "build", "--base=HEAD", "--dry-run"])
        .args(["--lane", "core", "--lane", "platform"])
        .output()
        .unwrap();
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    let core_at = human_stdout.find("Lane core:").unwrap();
    let platform_at = human_stdout.find("Lane platform:").unwrap();
    assert!(core_at < platform_at);
    assert!(human_stdout[..platform_at].contains("//src/elixir/core/accounts:build"));
    assert!(human_stdout[platform_at..].contains("//services/platform/gateway:build"));

    // Each lane runs on its own; several at once would hide which failed.
    let run = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "build", "--base=HEAD"])
        .args(["--lane", "core", "--lane", "platform"])
        .output()
        .unwrap();
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("--dry-run"));
}

#[test]
fn test_affected_lane_filters_after_dependents_expansion() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(
        &tmp,
        "src/elixir/core/accounts/package.json",
        r#"{"name":"accounts","scripts":{"build":"true"}}"#,
    );
    write_package_json(
        &tmp,
        "services/platform/gateway/package.json",
        r#"{"name":"gateway","dependencies":{"accounts":"file:../../../src/elixir/core/accounts"},"scripts":{"build":"true"}}"#,
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        "[affected.lanes.platform]\ninclude = [\"//services/platform/...\"]\n",
    );
    git_commit(&tmp, "Initial commit");
    write_fixture(&tmp, "src/elixir/core/accounts/change.js", "changed");

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "build",
            "--base=HEAD",
            "--dependents",
            "--lane",
            "platform",
            "--dry-run",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "lane failed: {output:?}");
    assert!(stdout.contains("//services/platform/gateway:build"));
    assert!(
        stdout.contains("//src/elixir/core/accounts:build  (target dependency)"),
        "expected filtered direct project to remain a target dependency: {stdout}"
    );
}

#[test]
fn test_affected_lane_validates_unknown_malformed_and_non_matching_selectors() {
    for (configuration, lane, expected) in [
        (
            "[affected.lanes.core]\ninclude = [\"src/elixir/core/...\"]\n",
            "core",
            "invalid selector 'src/elixir/core/...' in affected lane 'core' include",
        ),
        (
            "[affected.lanes.core]\ninclude = [\"//missing/...\"]\n",
            "core",
            "selector '//missing/...' in affected lane 'core' include matches no projects",
        ),
        (
            "[affected.lanes.core]\ninclude = [\"//src/elixir/core/...\"]\n",
            "other",
            "unknown affected lane 'other'; configured lanes: core",
        ),
    ] {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);
        write_mix_exs(
            &tmp,
            "src/elixir/core/accounts/mix.exs",
            "defmodule Accounts.MixProject do\n  use Mix.Project\n  def project, do: [app: :accounts, version: \"0.1.0\"]\nend\n",
        );
        write_aster_toml(&tmp, "aster.toml", configuration);
        git_commit(&tmp, "Initial commit");
        write_fixture(&tmp, "src/elixir/core/accounts/lib/change.ex", "changed");

        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(["affected", "build", "--base=HEAD", "--lane", lane])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "expected failure: {output:?}");
        assert!(stderr.contains(expected), "expected {expected:?}: {stderr}");
    }
}

#[test]
fn test_affected_lane_empty_selection_uses_existing_output_contracts() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(
        &tmp,
        "src/elixir/core/accounts/package.json",
        r#"{"name":"accounts","scripts":{"test":"true"}}"#,
    );
    write_package_json(
        &tmp,
        "services/platform/gateway/package.json",
        r#"{"name":"gateway","scripts":{"test":"true"}}"#,
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        "[affected.lanes.platform]\ninclude = [\"//services/platform/...\"]\n\n\
         [affected.lanes.core]\ninclude = [\"//src/elixir/core/...\"]\n",
    );
    git_commit(&tmp, "Initial commit");
    write_fixture(&tmp, "src/elixir/core/accounts/change.js", "changed");

    // A lane with nothing to run still has a plan among several.
    let together = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--json", "affected", "test", "--base=HEAD", "--dry-run"])
        .args(["--lane", "platform", "--lane", "core"])
        .output()
        .unwrap();
    assert!(together.status.success(), "lanes failed: {together:?}");
    let plans: serde_json::Value = serde_json::from_slice(&together.stdout).unwrap();
    assert_eq!(plans["lanes"]["platform"]["targets"], serde_json::json!([]));
    assert_eq!(plans["lanes"]["platform"]["count"], 0);
    assert!(plans["lanes"]["core"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|target| target["address"] == "//src/elixir/core/accounts:test"));
    let human = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--dry-run"])
        .args(["--lane", "platform", "--lane", "core"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&human.stdout).contains("Lane platform: no projects affected"));

    let human = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--lane", "platform"])
        .output()
        .unwrap();
    assert!(human.status.success());
    assert_eq!(
        String::from_utf8_lossy(&human.stdout).trim(),
        "No projects affected"
    );

    let json = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "--json",
            "affected",
            "test",
            "--base=HEAD",
            "--lane",
            "platform",
        ])
        .output()
        .unwrap();
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["summary"]["total"], 0);

    let quiet = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "--quiet",
            "affected",
            "test",
            "--base=HEAD",
            "--lane",
            "platform",
        ])
        .output()
        .unwrap();
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty() && quiet.stderr.is_empty());
}

/// Write two Node projects and a root aster.toml with two lanes; commit.
fn lane_matrix_workspace(tmp: &TempDir) {
    setup_git_repo(tmp);
    write_package_json(
        tmp,
        "services/api/package.json",
        r#"{"name":"api","scripts":{"test":"true"}}"#,
    );
    write_package_json(
        tmp,
        "services/web/package.json",
        r#"{"name":"web","scripts":{"test":"true"}}"#,
    );
    write_package_json(tmp, "docs/package.json", r#"{"name":"docs"}"#);
    write_aster_toml(
        tmp,
        "services/web/aster.toml",
        r#"
[targets.browser-e2e]
command = "true"
cache = { workspace_include = ["scripts/browser/**"] }
"#,
    );
    write_aster_toml(
        tmp,
        "aster.toml",
        r#"
[affected.lanes.api]
include = ["//services/api"]
matrix = { runs_on = "small" }

[affected.lanes.browser]
include = ["//services/web"]
name = "Browser proofs"
target = "browser-e2e"
toolchains = ["python"]
matrix = { runs_on = "large", browser = true }

[affected.lanes.everything]
include = ["//..."]
"#,
    );
    write_fixture(tmp, "scripts/browser/run.sh", "echo base");
    write_fixture(tmp, "README.md", "base");
    git_commit(tmp, "Initial commit");
}

fn aster_json(tmp: &TempDir, args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "aster {args:?} failed: {output:?}");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("aster {args:?} printed non-JSON ({e}): {output:?}"))
}

#[test]
fn matrix_has_a_row_for_each_lane_the_change_reaches() {
    let tmp = TempDir::new().unwrap();
    lane_matrix_workspace(&tmp);

    // A change inside services/api reaches the api lane and the catch-all
    // lane; the browser lane's project did not change.
    write_fixture(&tmp, "services/api/index.js", "changed");
    let rows = aster_json(&tmp, &["affected", "test", "--base=HEAD", "--matrix"]);
    assert_eq!(
        rows,
        serde_json::json!([
            {"lane": "api", "name": "api", "target": "test",
             "toolchains": ["nodejs"], "runs_on": "small"},
            {"lane": "everything", "name": "everything", "target": "test",
             "toolchains": ["nodejs"]},
        ])
    );

    // --lane limits the matrix to the lanes named.
    let rows = aster_json(
        &tmp,
        &[
            "affected",
            "test",
            "--base=HEAD",
            "--matrix",
            "--lane",
            "api",
        ],
    );
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["lane"], "api");
}

#[test]
fn a_changed_workspace_input_selects_the_lane_target_that_declares_it() {
    let tmp = TempDir::new().unwrap();
    lane_matrix_workspace(&tmp);

    // scripts/browser is outside every project; only browser-e2e declares it.
    write_fixture(&tmp, "scripts/browser/run.sh", "echo changed");
    let rows = aster_json(&tmp, &["affected", "test", "--base=HEAD", "--matrix"]);
    assert_eq!(
        rows,
        serde_json::json!([
            {"lane": "browser", "name": "Browser proofs", "target": "browser-e2e",
             "toolchains": ["nodejs", "python"], "runs_on": "large", "browser": true},
        ])
    );

    // The lane plan names the input as the reason, and under --related the
    // target runs in full because its own sources did not change.
    let plan = aster_json(
        &tmp,
        &[
            "--json",
            "affected",
            "test",
            "--base=HEAD",
            "--related",
            "--dry-run",
            "--lane",
            "browser",
        ],
    );
    let target = &plan["targets"][0];
    assert_eq!(target["address"], "//services/web:browser-e2e");
    assert_eq!(target["reason"], "affected");
    assert_eq!(
        target["files"],
        serde_json::json!(["scripts/browser/run.sh"])
    );
    assert!(
        target["selection"]
            .to_string()
            .contains("declared workspace input"),
        "{plan}"
    );

    // An unrelated root file selects nothing.
    write_fixture(&tmp, "scripts/browser/run.sh", "echo base");
    write_fixture(&tmp, "README.md", "changed");
    let rows = aster_json(&tmp, &["affected", "test", "--base=HEAD", "--matrix"]);
    assert_eq!(rows, serde_json::json!([]));
}

#[test]
fn matrix_rejects_reserved_fields_and_a_workspace_without_lanes() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name":"api","scripts":{"test":"true"}}"#,
    );
    write_aster_toml(&tmp, "aster.toml", "");
    git_commit(&tmp, "Initial commit");
    let run = |tmp: &TempDir| {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(["affected", "test", "--all", "--matrix"])
            .output()
            .unwrap()
    };

    // No lanes: an empty matrix would silently skip every job.
    let output = run(&tmp);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--matrix needs lanes"));

    // A matrix field named like a row field would be a duplicate JSON key.
    write_aster_toml(
        &tmp,
        "aster.toml",
        "[affected.lanes.api]\ninclude = [\"//services/api\"]\nmatrix = { name = \"x\" }\n",
    );
    let output = run(&tmp);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("matrix field 'name' is reserved"),
        "{output:?}"
    );
    assert!(
        stderr.contains("reserved: lane, name, target, toolchains"),
        "{output:?}"
    );
    assert!(!stderr.contains("set the lane's own"), "{output:?}");

    // `lane` is reserved too, and a lane has no `lane` key to set instead.
    // Issue #253: [affected.lanes.ci] include=["//a"], matrix = { lane = "x" }.
    write_package_json(
        &tmp,
        "a/package.json",
        r#"{"name":"a","scripts":{"test":"true"}}"#,
    );
    write_aster_toml(
        &tmp,
        "aster.toml",
        "[affected.lanes.ci]\ninclude = [\"//a\"]\nmatrix = { lane = \"x\" }\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--matrix"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("matrix field 'lane' is reserved"),
        "{output:?}"
    );
    assert!(
        stderr.contains("reserved: lane, name, target, toolchains"),
        "{output:?}"
    );
    assert!(stderr.contains("rename it"), "{output:?}");
    assert!(!stderr.contains("set the lane's own"), "{output:?}");
}

#[test]
fn all_selects_every_lane_project_that_defines_the_target_and_runs_it() {
    let tmp = TempDir::new().unwrap();
    lane_matrix_workspace(&tmp);

    // Nothing changed, yet --all plans every lane. docs has no test target,
    // so the catch-all lane runs only api and web.
    let rows = aster_json(&tmp, &["affected", "test", "--all", "--matrix"]);
    let lanes: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["lane"].as_str().unwrap())
        .collect();
    assert_eq!(lanes, ["api", "browser", "everything"]);
    let plan = aster_json(
        &tmp,
        &[
            "--json",
            "affected",
            "test",
            "--all",
            "--dry-run",
            "--lane",
            "everything",
        ],
    );
    let addresses: Vec<&str> = plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["reason"] != "target dependency")
        .map(|t| t["address"].as_str().unwrap())
        .collect();
    assert_eq!(addresses.len(), 2, "{plan}");
    assert!(addresses.contains(&"//services/api:test"));
    assert!(addresses.contains(&"//services/web:test"));

    // Running a lane with --all executes its target.
    let run = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--all",
            "--lane",
            "browser",
            "--no-cache",
        ])
        .output()
        .unwrap();
    assert!(run.status.success(), "run failed: {run:?}");
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("PASS //services/web:browser-e2e"),
        "{run:?}"
    );
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

// ============================================================================
// Target Execution Tests: Primary vs Dependency Projects
// ============================================================================

#[test]
fn test_target_only_runs_on_primary_projects() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Core library - test target FAILS (this proves it didn't run if overall succeeds)
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "echo core_build", "test": "exit 1"}}"#,
    );

    // API service depends on core - test target succeeds
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}, "scripts": {"build": "echo api_build", "test": "echo api_test"}}"#,
    );

    // Run test ONLY on api project - should SUCCEED because core:test shouldn't run
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["test", "//services/api"])
        .output()
        .unwrap();

    // If core:test ran, this would fail (exit 1). Success proves only api:test ran.
    assert!(
        output.status.success(),
        "Command should succeed - core:test should not run. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_target_on_all_runs_all_tests() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Core library - test target FAILS
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "echo core_build", "test": "exit 1"}}"#,
    );

    // API service (independent, no deps)
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "scripts": {"build": "echo api_build", "test": "echo api_test"}}"#,
    );

    // Run test on ALL projects - should FAIL because core:test fails
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["test", "--all"])
        .output()
        .unwrap();

    // With --all, core:test runs and fails
    assert!(
        !output.status.success(),
        "Command should fail - core:test should run with --all and it fails"
    );
}

#[test]
fn test_cached_targets_are_not_printed() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write_package_json(
        &tmp,
        "app/package.json",
        r#"{"name": "app", "scripts": {"build": "touch built.txt"}}"#,
    );

    let first = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "--all"])
        .output()
        .unwrap();
    assert!(first.status.success(), "Command failed: {first:?}");

    // The initial Node deps target creates package-lock.json, so run once more
    // after that input settles before asserting cache-hit behavior.
    let warm = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "--all"])
        .output()
        .unwrap();
    assert!(warm.status.success(), "Command failed: {warm:?}");

    let cached_json = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "--all", "--json"])
        .output()
        .unwrap();
    assert!(
        cached_json.status.success(),
        "Command failed: {cached_json:?}"
    );
    let cached_output: serde_json::Value =
        serde_json::from_slice(&cached_json.stdout).expect("valid JSON output");
    assert!(
        cached_output["summary"]["cached"].as_u64().unwrap_or(0) > 0,
        "Expected the second run to hit the cache: {cached_output}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "--all"])
        .output()
        .unwrap();
    assert!(output.status.success(), "Command failed: {output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(" cached"),
        "Cache-hit targets should not be printed: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 passed, 2 cached, 0 failed"),
        "Expected one consolidated cache count: {stdout}"
    );
}

#[test]
fn cache_key_includes_runtime_command_output() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name": "app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.build]
command = "true"
cache = { enabled = true, runtime = ["echo v1"] }
"#,
    );

    let first = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//app", "--no-deps"])
        .output()
        .unwrap();
    assert!(first.status.success(), "Command failed: {first:?}");

    let cache_path = tmp.path().join(".aster/cache/index/%2F%2Fapp.json");
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).unwrap()).unwrap();
    let hash_v1 = stored["entries"]["//app:build"]["key"]
        .as_str()
        .expect("cache hash")
        .to_string();

    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.build]
command = "true"
cache = { enabled = true, runtime = ["echo v2"] }
"#,
    );
    let second = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//app", "--no-deps"])
        .output()
        .unwrap();
    assert!(second.status.success(), "Command failed: {second:?}");

    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).unwrap()).unwrap();
    let hash_v2 = stored["entries"]["//app:build"]["key"]
        .as_str()
        .expect("cache hash")
        .to_string();
    assert_ne!(
        hash_v1, hash_v2,
        "runtime command output must change the cache key"
    );
}

#[test]
fn test_verbose_shows_primary_project_count() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Core library
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "echo build", "test": "echo test"}}"#,
    );

    // API depends on core
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}, "scripts": {"build": "echo build", "test": "echo test"}}"#,
    );

    // Run with verbose to check message
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--verbose", "test", "//services/api"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verbose message should show 1 project (primary only), not 2
    assert!(
        stderr.contains("Running 'test' on 1 project"),
        "Expected 'Running test on 1 project' (primary only), got: {stderr}"
    );
}

#[test]
fn test_dependency_build_targets_still_run() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Core library - build FAILS (to prove it runs)
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "exit 1", "test": "echo test"}}"#,
    );

    // API depends on core
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}, "scripts": {"build": "echo build", "test": "echo test"}}"#,
    );

    // Run test on api - should FAIL because core:build fails (it's a target-level dep)
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["test", "//services/api"])
        .output()
        .unwrap();

    // core:build should still run (target-level dep of api:test) and fail
    assert!(
        !output.status.success(),
        "Command should fail - core:build is a target-level dep and it fails"
    );
}

#[test]
fn test_no_deps_skips_all_dependency_targets() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Core library - both build and test FAIL
    write_package_json(
        &tmp,
        "libs/core/package.json",
        r#"{"name": "core", "scripts": {"build": "exit 1", "test": "exit 1"}}"#,
    );

    // API depends on core
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}, "scripts": {"build": "echo build", "test": "echo test"}}"#,
    );

    // Run test on api with --no-deps - should SUCCEED because nothing from core runs
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["test", "//services/api", "--no-deps"])
        .output()
        .unwrap();

    // With --no-deps, core is completely excluded, so api:test runs alone
    assert!(
        output.status.success(),
        "Command should succeed with --no-deps - core targets should not run. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_workspace_member_resolves_root_deps_target() {
    // Regression test: running a target on an npm workspace member must resolve
    // target-level dependencies on the workspace root (e.g., //src/ts:deps).
    //
    // The workspace root project is NOT a project-level dependency of the member,
    // so expand_selection won't include it. The executor must receive all discovered
    // projects to resolve the cross-project target-level dependency.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Workspace root with workspaces field
    // The deps target is auto-generated by the NodeJS plugin (runs npm install).
    // We add a package-lock.json so the plugin uses `npm ci` which will fail
    // because there's no real node_modules setup - but more importantly we just
    // need to verify the target IS scheduled. Use aster.toml to override deps
    // with a failing command.
    write_package_json(
        &tmp,
        "src/ts/package.json",
        r#"{"name": "ts-root", "workspaces": ["packages/*"]}"#,
    );
    write_aster_toml(
        &tmp,
        "src/ts/aster.toml",
        "[targets.deps]\ncommand = \"exit 1\"\n",
    );

    // Workspace member - build would succeed on its own
    write_package_json(
        &tmp,
        "src/ts/packages/sdk/package.json",
        r#"{"name": "sdk", "scripts": {"build": "echo build_ok"}}"#,
    );

    // Run build on the workspace member - should FAIL because //src/ts:deps fails
    // (the member's :build target depends on //src/ts:deps via workspace detection)
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//src/ts/packages/sdk", "--no-cache"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "Should fail because workspace root :deps is a target-level dep and it fails. stderr: {stderr}"
    );
    assert!(
        stderr.contains("//src/ts:deps"),
        "The workspace root deps target should appear in the output. stderr: {stderr}"
    );
}

#[test]
fn test_workspace_member_no_deps_skips_root_deps_target() {
    // With --no-deps, the workspace root's :deps target should NOT run,
    // even though it's a target-level dependency of the member's :build.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Workspace root - override deps to fail
    write_package_json(
        &tmp,
        "src/ts/package.json",
        r#"{"name": "ts-root", "workspaces": ["packages/*"]}"#,
    );
    write_aster_toml(
        &tmp,
        "src/ts/aster.toml",
        "[targets.deps]\ncommand = \"exit 1\"\n",
    );

    // Workspace member
    write_package_json(
        &tmp,
        "src/ts/packages/sdk/package.json",
        r#"{"name": "sdk", "scripts": {"build": "echo build_ok"}}"#,
    );

    // Run build with --no-deps - should SUCCEED because deps target is skipped
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//src/ts/packages/sdk", "--no-cache", "--no-deps"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "With --no-deps, workspace root :deps should not run. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_cross_project_target_dep_resolved_for_non_dependency_project() {
    // Regression test: target-level dependencies that reference projects outside
    // the project-level dependency graph must still be resolved.
    //
    // This tests the general case using aster.toml to create a target-level
    // cross-project dependency without a project-level dependency.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Shared infra project - its "setup" target FAILS to prove it runs
    write_package_json(
        &tmp,
        "infra/package.json",
        r#"{"name": "infra", "scripts": {"setup": "exit 1"}}"#,
    );
    // Also define via aster.toml in case plugin doesn't pick up custom script names
    write_aster_toml(
        &tmp,
        "infra/aster.toml",
        "[targets.setup]\ncommand = \"exit 1\"\n",
    );

    // App project - build depends on infra:setup via aster.toml (target-level only)
    write_package_json(
        &tmp,
        "app/package.json",
        r#"{"name": "app", "scripts": {"build": "echo build_ok"}}"#,
    );
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        "[targets.build]\ncommand = \"echo build_ok\"\ndepends_on = [\"//infra:setup\"]\n",
    );

    // Run build on app - should FAIL because //infra:setup fails
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//app", "--no-cache"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "Should fail because //infra:setup is a target-level dep and it fails. stderr: {stderr}"
    );
    assert!(
        stderr.contains("//infra:setup"),
        "The infra setup target should appear in the output. stderr: {stderr}"
    );
}

// ============================================================================
// Affected: Target-Level Dependency Resolution
// ============================================================================

#[test]
fn test_affected_resolves_target_level_dependencies() {
    // Regression test: the affected command must resolve target-level dependencies
    // even for projects that are NOT directly affected.
    //
    // Scenario: app:build depends on sdk:build (target-level dep via file: dependency).
    // Only app has changes (sdk is NOT affected). The executor must still run sdk:build
    // because it's a target-level dependency of app:build.
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // SDK library - build FAILS (proves it runs even when not directly affected)
    write_package_json(
        &tmp,
        "libs/sdk/package.json",
        r#"{"name": "sdk", "scripts": {"build": "exit 1"}}"#,
    );

    // App depends on SDK (creates target-level dep: app:build -> sdk:build)
    write_package_json(
        &tmp,
        "services/app/package.json",
        r#"{"name": "app", "dependencies": {"sdk": "file:../../libs/sdk"}, "scripts": {"build": "echo app_build"}}"#,
    );

    git_commit(&tmp, "Initial commit");

    // Only change the app project (sdk is NOT directly affected)
    fs::write(tmp.path().join("services/app/new_file.txt"), "change").unwrap();

    // Run affected build - should FAIL because sdk:build fails
    // sdk:build is a target-level dep of app:build, even though sdk isn't directly affected
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "build", "--base=HEAD"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Should fail because sdk:build (target-level dep) runs even though sdk isn't directly affected. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_affected_with_dependents_resolves_target_deps() {
    // When --dependents is used, the full graph should include:
    // 1. Directly affected projects
    // 2. Their ProjectGraph dependents (reverse deps)
    // 3. Target-level dependencies for ALL of the above
    //
    // Scenario: sdk changed. With --dependents, app (which depends on sdk) is added.
    // Both run build. sdk:build is also a target-level dep of app:build, but it's
    // already primary so it runs the requested target directly.
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // SDK library - build succeeds
    write_package_json(
        &tmp,
        "libs/sdk/package.json",
        r#"{"name": "sdk", "scripts": {"build": "echo sdk_build"}}"#,
    );

    // Unrelated library - build FAILS (proves it runs when it's a target dep of a dependent)
    write_package_json(
        &tmp,
        "libs/infra/package.json",
        r#"{"name": "infra", "scripts": {"build": "exit 1"}}"#,
    );

    // App depends on both sdk and infra
    write_package_json(
        &tmp,
        "services/app/package.json",
        r#"{"name": "app", "dependencies": {"sdk": "file:../../libs/sdk", "infra": "file:../../libs/infra"}, "scripts": {"build": "echo app_build"}}"#,
    );

    git_commit(&tmp, "Initial commit");

    // Only change sdk (app is NOT directly affected, but IS a dependent)
    fs::write(tmp.path().join("libs/sdk/new_file.txt"), "change").unwrap();

    // Run affected build with --dependents
    // - sdk is directly affected → primary
    // - app is a dependent of sdk → primary (runs build)
    // - infra is a target-level dep of app:build → should also run
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "build", "--base=HEAD", "--dependents"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify affected projects are listed
    assert!(
        stdout.contains("//libs/sdk"),
        "Expected //libs/sdk in affected output: {stdout}"
    );
    assert!(
        stdout.contains("//services/app"),
        "Expected //services/app (dependent) in affected output: {stdout}"
    );

    // infra:build should have run (target-level dep of app:build) and failed
    assert!(
        !output.status.success(),
        "Should fail because infra:build (target-level dep of dependent app) fails. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ============================================================================
// Dry Run: Full Execution Graph with Rationale
// ============================================================================

#[test]
fn test_dry_run_shows_full_target_graph_with_rationale() {
    // Dry run should show ALL targets that would execute (not just affected projects),
    // with rationale: affected (files changed), dependent, or target dependency.
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // SDK library (will NOT be directly affected, but IS a target dependency)
    write_package_json(
        &tmp,
        "libs/sdk/package.json",
        r#"{"name": "sdk", "scripts": {"build": "echo sdk_build"}}"#,
    );

    // App depends on SDK
    write_package_json(
        &tmp,
        "services/app/package.json",
        r#"{"name": "app", "dependencies": {"sdk": "file:../../libs/sdk"}, "scripts": {"build": "echo app_build"}}"#,
    );

    git_commit(&tmp, "Initial commit");

    // Only change the app project
    fs::create_dir_all(tmp.path().join("services/app/src")).unwrap();
    fs::write(
        tmp.path().join("services/app/src/index.ts"),
        "export default 1;",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "build", "--base=HEAD", "--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Dry run failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show the affected target with rationale
    assert!(
        stdout.contains("//services/app:build") && stdout.contains("affected"),
        "Expected //services/app:build marked as affected: {stdout}"
    );

    // Should show target dependencies (sdk:build needed by app:build)
    assert!(
        stdout.contains("//libs/sdk:build") && stdout.contains("target dependency"),
        "Expected //libs/sdk:build marked as target dependency: {stdout}"
    );

    // Should show the changed file (git may show directory for new untracked dirs)
    assert!(
        stdout.contains("services/app/src"),
        "Expected changed file listed: {stdout}"
    );

    // Should show dependency target count
    assert!(
        stdout.contains("dependency targets"),
        "Expected 'dependency targets' in summary: {stdout}"
    );
}

#[test]
fn test_dry_run_shows_dependents_rationale() {
    // With --dependents, dependent projects should be labeled as "dependent"
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // SDK library
    write_package_json(
        &tmp,
        "libs/sdk/package.json",
        r#"{"name": "sdk", "scripts": {"build": "echo sdk_build"}}"#,
    );

    // App depends on SDK
    write_package_json(
        &tmp,
        "services/app/package.json",
        r#"{"name": "app", "dependencies": {"sdk": "file:../../libs/sdk"}, "scripts": {"build": "echo app_build"}}"#,
    );

    git_commit(&tmp, "Initial commit");

    // Change SDK (app will be added as a dependent)
    fs::create_dir_all(tmp.path().join("libs/sdk/src")).unwrap();
    fs::write(
        tmp.path().join("libs/sdk/src/index.ts"),
        "export default 1;",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "build",
            "--base=HEAD",
            "--dependents",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Dry run failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // SDK should be marked as affected
    assert!(
        stdout.contains("//libs/sdk:build") && stdout.contains("affected"),
        "Expected //libs/sdk:build marked as affected: {stdout}"
    );

    // App should be marked as dependent
    assert!(
        stdout.contains("//services/app:build") && stdout.contains("dependent"),
        "Expected //services/app:build marked as dependent: {stdout}"
    );
}

// ============================================================================
// Full Logs Tests
// ============================================================================

#[test]
fn test_full_logs_flag_on_affected() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // Create a project with a test that outputs many lines and fails
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "scripts": {"test": "for i in $(seq 1 30); do echo line_$i; done && exit 1"}}"#,
    );
    git_commit(&tmp, "Initial commit");

    // Make a change to trigger affected
    fs::write(tmp.path().join("services/api/new_file.txt"), "change").unwrap();

    // Without --full-logs, output is truncated to 15 lines
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    let stderr_without = String::from_utf8_lossy(&output.stderr);

    // Should NOT contain early lines (they are truncated)
    assert!(
        !stderr_without.contains("line_1\n") || stderr_without.contains("line_16"),
        "Without --full-logs, early lines should be truncated"
    );

    // With --full-logs, should see all lines
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--full-logs"])
        .output()
        .unwrap();

    let stderr_with = String::from_utf8_lossy(&output.stderr);

    // Should contain early lines
    assert!(
        stderr_with.contains("line_1"),
        "With --full-logs, should show line_1: {stderr_with}"
    );
    assert!(
        stderr_with.contains("line_30"),
        "With --full-logs, should show line_30: {stderr_with}"
    );
}

#[test]
fn test_full_logs_flag_position() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "scripts": {"test": "echo test_output && exit 1"}}"#,
    );
    git_commit(&tmp, "Initial commit");

    fs::write(tmp.path().join("services/api/new_file.txt"), "change").unwrap();

    // Test --full-logs before subcommand
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--full-logs", "affected", "test", "--base=HEAD"])
        .output()
        .unwrap();

    // Should not fail with "unknown flag" error
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "Flag before subcommand should work: {stderr}"
    );

    // Test --full-logs after subcommand args
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--full-logs"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "Flag after subcommand should work: {stderr}"
    );
}

// ============================================================================
// Language Filter (--lang)
// ============================================================================

#[test]
fn test_list_lang_filter() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Node.js project
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "version": "1.0.0"}"#,
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

    // Filter to only Python
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "python"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("//libs/ml"),
        "Expected Python project in output: {stdout}"
    );
    assert!(
        !stdout.contains("//services/api"),
        "Node.js project should be filtered out: {stdout}"
    );
    assert!(
        !stdout.contains("//services/backend"),
        "Elixir project should be filtered out: {stdout}"
    );
}

#[test]
fn test_list_lang_filter_multiple() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "version": "1.0.0"}"#,
    );

    write_pyproject_toml(
        &tmp,
        "libs/ml/pyproject.toml",
        r#"
[project]
name = "ml"
version = "1.0.0"
"#,
    );

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

    // Filter to nodejs and elixir (comma-separated)
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "nodejs,elixir"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("//services/api"),
        "Expected Node.js project: {stdout}"
    );
    assert!(
        stdout.contains("//services/backend"),
        "Expected Elixir project: {stdout}"
    );
    assert!(
        !stdout.contains("//libs/ml"),
        "Python project should be filtered out: {stdout}"
    );
}

#[test]
fn test_lang_filter_invalid_language() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write_package_json(&tmp, "services/api/package.json", r#"{"name": "api"}"#);

    // Invalid language should produce an error
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["list", "--lang", "javascript"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Should fail with invalid language"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown language 'javascript'"),
        "Expected helpful error message: {stderr}"
    );
    assert!(
        stderr.contains("nodejs"),
        "Error should list valid languages: {stderr}"
    );
}

#[test]
fn test_target_lang_filter() {
    // Test --lang with external subcommand (aster test --all --lang nodejs)
    // Python project has a test that fails, Node.js project passes.
    // If lang filter works, only nodejs runs and we get success.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "scripts": {"test": "echo api_test_pass"}}"#,
    );

    write_pyproject_toml(
        &tmp,
        "libs/ml/pyproject.toml",
        r#"
[project]
name = "ml"
version = "1.0.0"
"#,
    );
    // Create pytest marker so test target is detected
    fs::create_dir_all(tmp.path().join("libs/ml/tests")).unwrap();
    fs::write(tmp.path().join("libs/ml/tests/test_ml.py"), "").unwrap();

    // Without --lang, all projects are selected
    let output_all = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["test", "--all"])
        .output()
        .unwrap();
    let stdout_all = String::from_utf8_lossy(&output_all.stdout);
    assert!(
        stdout_all.contains("2 passed") || stdout_all.contains("3 projects"),
        "Without --lang should run multiple projects: {stdout_all}"
    );

    // With --lang nodejs, only Node.js projects selected
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["--verbose", "test", "--all", "--lang", "nodejs"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verbose stderr shows how many primary projects run, stdout has summary
    // The Python project should not appear
    assert!(
        !stdout.contains("//libs/ml"),
        "Python project should be filtered out: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn test_target_lang_filter_invalid() {
    // Test --lang with invalid language in external subcommand
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "scripts": {"test": "echo test"}}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["test", "--all", "--lang", "js"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Should fail with invalid language"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown language 'js'"),
        "Expected helpful error message: {stderr}"
    );
}

// ============================================================================
// --only-affected-files bug: falls back to full suite when no test files changed
// ============================================================================

#[test]
fn test_only_affected_files_skips_when_no_test_files_changed() {
    // When --only-affected-files is used and only source files (not test files)
    // are in the change set, the project's test target should be skipped rather
    // than running the full test suite.

    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // Python project with explicit test target that has files_list capability
    write_pyproject_toml(
        &tmp,
        "libs/mylib/pyproject.toml",
        r#"
[project]
name = "mylib"
version = "1.0.0"
requires-python = ">=3.8"

[project.optional-dependencies]
test = ["pytest"]
"#,
    );

    // Add a test file and a source file in initial commit
    fs::create_dir_all(tmp.path().join("libs/mylib/src")).unwrap();
    fs::write(
        tmp.path().join("libs/mylib/src/utils.py"),
        "def add(a, b): return a + b",
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("libs/mylib/tests")).unwrap();
    fs::write(
        tmp.path().join("libs/mylib/tests/test_utils.py"),
        "def test_add(): assert True",
    )
    .unwrap();
    git_commit(&tmp, "Initial commit");

    // Now change ONLY a source file (not a test file)
    fs::write(
        tmp.path().join("libs/mylib/src/utils.py"),
        "def add(a, b): return a + b\ndef sub(a, b): return a - b",
    )
    .unwrap();

    // Run affected test with --only-affected-files
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--base=HEAD",
            "--only-affected-files",
            "--json",
            "--no-cache",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Parse JSON output
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("Failed to parse JSON output: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });

    let results = json["results"].as_array().expect("results should be array");
    let mylib_result = results.iter().find(|r| {
        r["address"]
            .as_str()
            .map(|a| a.contains("mylib"))
            .unwrap_or(false)
    });

    // With --only-affected-files, since no test files changed, the project's
    // test target should NOT be executed at all.
    assert!(
        mylib_result.is_none(),
        "With --only-affected-files and no test files changed, mylib:test should not run. \
         Got: {mylib_result:?}"
    );
}

#[test]
fn test_only_affected_files_runs_when_test_files_changed() {
    // When --only-affected-files is used and test files ARE in the change set,
    // the project's test target should run with only those files.

    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    // Python project
    write_pyproject_toml(
        &tmp,
        "libs/mylib/pyproject.toml",
        r#"
[project]
name = "mylib"
version = "1.0.0"
requires-python = ">=3.8"

[project.optional-dependencies]
test = ["pytest"]
"#,
    );

    // Add source and test files in initial commit
    fs::create_dir_all(tmp.path().join("libs/mylib/src")).unwrap();
    fs::write(
        tmp.path().join("libs/mylib/src/utils.py"),
        "def add(a, b): return a + b",
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("libs/mylib/tests")).unwrap();
    fs::write(
        tmp.path().join("libs/mylib/tests/test_utils.py"),
        "def test_add(): assert True",
    )
    .unwrap();
    git_commit(&tmp, "Initial commit");

    // Change a test file
    fs::write(
        tmp.path().join("libs/mylib/tests/test_utils.py"),
        "def test_add(): assert 1 + 1 == 2",
    )
    .unwrap();

    // Run affected test with --only-affected-files
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--base=HEAD",
            "--only-affected-files",
            "--json",
            "--no-cache",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Parse JSON output
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("Failed to parse JSON output: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });

    let results = json["results"].as_array().expect("results should be array");
    let mylib_result = results.iter().find(|r| {
        r["address"]
            .as_str()
            .map(|a| a.contains("mylib"))
            .unwrap_or(false)
    });

    // When test files are changed, the target SHOULD run
    assert!(
        mylib_result.is_some(),
        "With --only-affected-files and test files changed, mylib:test should run. \
         stdout: {stdout}, stderr: {stderr}"
    );
}

/// Write an executable `record.sh` at the workspace root. Each run writes its
/// file arguments to `<name>.args` beside the script, so a test can tell
/// whether a target ran and with which files.
fn write_recorder(tmp: &TempDir) {
    write_fixture(
        tmp,
        "record.sh",
        "#!/bin/sh\nname=$1\nshift\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/$name.args\"\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp.path().join("record.sh");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn test_only_affected_files_runs_dependents_in_full() {
    // web depends on core. Both test targets take a files list.
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_recorder(&tmp);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_aster_toml(
        &tmp,
        "libs/core/aster.toml",
        r#"
[targets.test]
command = "../../record.sh core {files}"
capabilities = ["files_list"]
cache = { enabled = false }
"#,
    );
    write_package_json(
        &tmp,
        "apps/web/package.json",
        r#"{"name": "web", "dependencies": {"core": "file:../../libs/core"}}"#,
    );
    write_aster_toml(
        &tmp,
        "apps/web/aster.toml",
        r#"
[targets.test]
command = "../../record.sh web {files}"
capabilities = ["files_list"]
cache = { enabled = false }
"#,
    );
    git_commit(&tmp, "Initial commit");

    // Only core changes. web is selected through --dependents and has no
    // changed files of its own.
    write_fixture(&tmp, "libs/core/index.js", "export const x = 1;\n");
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--base=HEAD",
            "--dependents",
            "--only-affected-files",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "aster failed: {output:?}");

    // core runs on its changed file; web runs its whole suite (the
    // placeholder expands to nothing) instead of being skipped.
    assert_eq!(
        fs::read_to_string(tmp.path().join("core.args")).unwrap(),
        "index.js\n"
    );
    let web_args = fs::read_to_string(tmp.path().join("web.args")).unwrap_or_else(|_| {
        panic!(
            "web:test did not run; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(web_args.trim(), "");
}

/// web depends on core, and both change in one commit. Returns the file
/// arguments each `test` target ran with under `--only-affected-files`.
#[cfg(unix)]
fn only_affected_files_with_a_changed_dependency(flags: &[&str]) -> (String, String) {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_recorder(&tmp);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_aster_toml(
        &tmp,
        "libs/core/aster.toml",
        "[targets.test]\ncommand = \"../../record.sh core {files}\"\ncapabilities = [\"files_list\"]\ncache = { enabled = false }\n",
    );
    write_package_json(
        &tmp,
        "apps/web/package.json",
        r#"{"name": "web", "dependencies": {"core": "file:../../libs/core"}}"#,
    );
    write_aster_toml(
        &tmp,
        "apps/web/aster.toml",
        "[targets.test]\ncommand = \"../../record.sh web {files}\"\ncapabilities = [\"files_list\"]\ncache = { enabled = false }\n",
    );
    git_commit(&tmp, "Initial commit");

    write_fixture(&tmp, "libs/core/index.js", "export const x = 1;\n");
    write_fixture(&tmp, "apps/web/page.js", "export const y = 2;\n");
    let mut args = vec!["affected", "test", "--base=HEAD", "--only-affected-files"];
    args.extend(flags);
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(&args)
        .output()
        .unwrap();
    assert!(output.status.success(), "aster failed: {output:?}");
    let ran = |name: &str| {
        fs::read_to_string(tmp.path().join(format!("{name}.args")))
            .unwrap()
            .trim()
            .to_string()
    };
    (ran("core"), ran("web"))
}

#[cfg(unix)]
#[test]
fn test_only_affected_files_keeps_its_own_files_when_a_dependency_also_changed() {
    // Without --dependents the run is about each project's own change: a
    // project that only depends on core is not selected at all, so one that
    // also has a changed file runs on that file.
    let (core, web) = only_affected_files_with_a_changed_dependency(&[]);
    assert_eq!(core, "index.js");
    assert_eq!(web, "page.js");
}

#[cfg(unix)]
#[test]
fn test_only_affected_files_with_dependents_runs_in_full_when_a_dependency_also_changed() {
    // Under --dependents web answers for the core change too, which its own
    // file list does not describe, so it runs in full.
    let (core, web) = only_affected_files_with_a_changed_dependency(&["--dependents"]);
    assert_eq!(core, "index.js");
    assert_eq!(web, "");
}

#[cfg(unix)]
#[test]
fn test_only_affected_files_reaches_a_same_project_dependency_target() {
    // CI calls a wrapper target (test-ci) whose only job is to depend on
    // the file-aware test target.
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_recorder(&tmp);
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_aster_toml(
        &tmp,
        "libs/core/aster.toml",
        r#"
[targets.test]
command = "../../record.sh core {files}"
capabilities = ["files_list"]
files_glob = "*.test.js"
cache = { enabled = false }

[targets.test-ci]
command = "true"
depends_on = ["//self:test"]
cache = { enabled = false }
"#,
    );
    git_commit(&tmp, "Initial commit");

    write_fixture(&tmp, "libs/core/a.test.js", "test\n");
    write_fixture(&tmp, "libs/core/a.js", "code\n");
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test-ci",
            "--base=HEAD",
            "--only-affected-files",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "aster failed: {output:?}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("core.args")).unwrap(),
        "a.test.js\n"
    );

    // With no matching file the dependency target is skipped, while the
    // wrapper still runs.
    git_commit(&tmp, "Add files");
    write_fixture(&tmp, "libs/core/a.js", "changed\n");
    fs::remove_file(tmp.path().join("core.args")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "--json",
            "affected",
            "test-ci",
            "--base=HEAD",
            "--only-affected-files",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "aster failed: {output:?}");
    assert!(!tmp.path().join("core.args").exists());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let status_of = |address: &str| {
        json["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["address"] == address)
            .map(|r| r["status"].as_str().unwrap().to_string())
    };
    assert_eq!(status_of("//libs/core:test").as_deref(), Some("skipped"));
    assert_eq!(status_of("//libs/core:test-ci").as_deref(), Some("passed"));
}

/// The canonical end-to-end story for Rust related-test selection: a real
/// crate, a real git change, aster choosing the tests, and cargo running
/// them.
#[test]
fn test_only_affected_files_runs_rust_tests_related_to_the_change() {
    // Setup: a crate where `a` calls `b`, `b` calls `c`, and `d` stands
    // alone. Each module has one unit test, and each integration test
    // exercises one module through the crate's public path.
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);
    write_fixture(
        &tmp,
        "crates/chain/Cargo.toml",
        "[package]\nname = \"chain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );
    write_fixture(
        &tmp,
        "crates/chain/src/lib.rs",
        "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
    );
    for (module, body) in [
        ("a", "crate::b::b() + 1"),
        ("b", "super::c::c() + 1"),
        ("c", "1"),
        ("d", "7"),
    ] {
        write_fixture(
            &tmp,
            &format!("crates/chain/src/{module}.rs"),
            &format!(
                "pub fn {module}() -> u32 {{ {body} }}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn {module}_works() {{\n        assert!(super::{module}() > 0);\n    }}\n}}\n"
            ),
        );
    }
    write_fixture(
        &tmp,
        "crates/chain/tests/through_a.rs",
        "#[test]\nfn a_is_three() { assert_eq!(chain::a::a(), 3); }\n",
    );
    write_fixture(
        &tmp,
        "crates/chain/tests/through_d.rs",
        "#[test]\nfn d_is_seven() { assert_eq!(chain::d::d(), 7); }\n",
    );
    write_fixture(&tmp, "crates/chain/.gitignore", "target/\n");
    git_commit(&tmp, "Initial commit");

    // Change: the leaf module c.
    write_fixture(
        &tmp,
        "crates/chain/src/c.rs",
        "pub fn c() -> u32 { 2 - 1 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn c_works() {\n        assert!(super::c() > 0);\n    }\n}\n",
    );

    // The dry run explains the selection without running cargo.
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "affected",
            "test",
            "--base=HEAD",
            "--only-affected-files",
            "--dry-run",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "dry run failed: {output:?}");
    assert!(
        stdout.contains("command: cargo test -p chain --lib -- a:: b:: c::"),
        "dry run should show the narrowed unit-test command: {stdout}"
    );
    assert!(
        stdout.contains("command: cargo test -p chain --test through_a"),
        "dry run should select only the integration test that reaches c: {stdout}"
    );

    // Run: aster executes the narrowed commands through cargo.
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("CARGO_TARGET_DIR", tmp.path().join("target"))
        .args([
            "--json",
            "affected",
            "test",
            "--base=HEAD",
            "--only-affected-files",
            "--no-cache",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "aster failed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let test_output = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["address"] == "//crates/chain:test")
        .and_then(|r| r["output"].as_str())
        .unwrap_or_else(|| panic!("no output for //crates/chain:test: {stdout}"))
        .to_string();

    // Observe: the tests of every module that reaches c ran, plus the one
    // integration test through a; d's unit and integration tests did not.
    for ran in [
        "a::tests::a_works",
        "b::tests::b_works",
        "c::tests::c_works",
        "a_is_three",
    ] {
        assert!(
            test_output.contains(&format!("test {ran} ... ok")),
            "{ran} should run: {test_output}"
        );
    }
    for skipped in ["d_works", "d_is_seven"] {
        assert!(
            !test_output.contains(skipped),
            "{skipped} should not run: {test_output}"
        );
    }
}

fn cacheable_app(tmp: &TempDir) {
    setup_workspace(tmp);
    write_package_json(tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        tmp,
        "app/aster.toml",
        r#"
[targets.build]
command = "true"
cache = { enabled = true, include = ["package.json"] }
"#,
    );
}

#[test]
fn test_aster_cache_dir_env_stores_cache_outside_workspace() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    cacheable_app(&tmp);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_CACHE_DIR", outside.path())
        .args(["build", "//app", "--no-deps"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let outside_cache = fs::read_to_string(outside.path().join("index/%2F%2Fapp.json")).unwrap();
    assert!(
        outside_cache.contains("//app:build"),
        "expected cache entry outside the workspace: {outside_cache}"
    );
    assert!(
        !tmp.path().join(".aster/cache/cache.json").exists(),
        "env override must not write .aster/cache/cache.json"
    );
    assert!(
        !tmp.path().join(".aster/cache.json").exists(),
        "env override must not write the legacy .aster/cache.json"
    );

    let status = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_CACHE_DIR", outside.path())
        .args(["cache", "status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success(),
        "cache status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        stdout.contains(&format!("Cache dir: {}", outside.path().display())),
        "status should print the resolved dir: {stdout}"
    );
}

#[test]
fn test_cache_dir_flag_overrides_env() {
    let tmp = TempDir::new().unwrap();
    let from_env = TempDir::new().unwrap();
    let from_flag = TempDir::new().unwrap();
    cacheable_app(&tmp);

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_CACHE_DIR", from_env.path())
        .args([
            "--cache-dir",
            from_flag.path().to_str().unwrap(),
            "build",
            "//app",
            "--no-deps",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let flag_cache = fs::read_to_string(from_flag.path().join("index/%2F%2Fapp.json")).unwrap();
    assert!(
        flag_cache.contains("//app:build"),
        "flag dir should hold the entry: {flag_cache}"
    );
    assert!(
        !from_env.path().join("index/%2F%2Fapp.json").exists(),
        "--cache-dir must override ASTER_CACHE_DIR"
    );
    assert!(!tmp.path().join(".aster/cache/cache.json").exists());

    let status = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "--cache-dir",
            from_flag.path().to_str().unwrap(),
            "cache",
            "status",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains(&format!("Cache dir: {}", from_flag.path().display())),
        "status should print the flag dir: {stdout}"
    );

    let json_status = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args([
            "--json",
            "--cache-dir",
            from_flag.path().to_str().unwrap(),
            "cache",
            "status",
        ])
        .output()
        .unwrap();
    let json_stdout = String::from_utf8_lossy(&json_status.stdout);
    assert!(
        !json_stdout.contains("Cache dir:"),
        "JSON status must stay unchanged: {json_stdout}"
    );
    let parsed: serde_json::Value = serde_json::from_str(json_stdout.trim()).unwrap();
    assert!(parsed["targets"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row["address"] == "//app:build")));
}

/// A Cargo workspace member's cache key includes the root Cargo.lock, so
/// editing that file changes both members' stored hashes.
#[test]
fn cargo_workspace_root_lockfile_invalidates_member_cache_keys() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_fixture(
        &tmp,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/one\", \"crates/two\"]\n",
    );
    write_fixture(&tmp, "Cargo.lock", "# lock v1\n");
    for name in ["one", "two"] {
        write_fixture(
            &tmp,
            &format!("crates/{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
        write_fixture(
            &tmp,
            &format!("crates/{name}/src/lib.rs"),
            "pub fn ready() {}",
        );
        write_fixture(
            &tmp,
            &format!("crates/{name}/aster.toml"),
            "[targets.stamp]\ncommand = \"true\"\ndepends_on = []\ncache.enabled = true\n",
        );
    }

    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "aster {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run(&["stamp", "--all"]);
    let hash = |address: &str| {
        let output = run(&["--json", "cache", "status", address]);
        let entry: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("cache status json");
        entry["targets"][0]["key"]
            .as_str()
            .unwrap_or_else(|| panic!("missing key for {address}: {entry}"))
            .to_string()
    };
    let one = hash("//crates/one:stamp");
    let two = hash("//crates/two:stamp");

    write_fixture(&tmp, "Cargo.lock", "# lock v2\n");
    run(&["stamp", "--all"]);
    let one_after = hash("//crates/one:stamp");
    let two_after = hash("//crates/two:stamp");

    assert_ne!(
        one, one_after,
        "root Cargo.lock must change //crates/one:stamp"
    );
    assert_ne!(
        two, two_after,
        "root Cargo.lock must change //crates/two:stamp"
    );
}

/// Run `//app:build` and return the hash stored for it.
///
/// The fixture defines `build` without the implicit `//self:deps` edge, so
/// `--no-deps` writes a cache entry without running `npm install`. Plugin
/// cache inputs still come from the nodejs plugin.
fn cached_build_hash(tmp: &TempDir, ci: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aster"));
    cmd.current_dir(tmp.path())
        .args(["run", "//app:build", "--no-deps"])
        .env_remove("CI");
    if let Some(value) = ci {
        cmd.env("CI", value);
    }
    let run = cmd.output().unwrap();
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let status = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["cache", "status", "//app:build", "--json"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "cache status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8(status.stdout).unwrap();
    let entry: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("cache status was not an entry ({err}): {stdout}"));
    entry["targets"][0]["key"]
        .as_str()
        .unwrap_or_else(|| panic!("cache status has no key: {stdout}"))
        .to_string()
}

#[test]
fn test_ci_env_var_does_not_change_cache_key() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.build]
command = "true"
"#,
    );

    let unset = cached_build_hash(&tmp, None);
    let set = cached_build_hash(&tmp, Some("1"));
    assert_eq!(
        unset, set,
        "CI=1 must not change the cache key of a default plugin target"
    );
    assert!(!unset.is_empty());
}

#[test]
fn test_ci_env_var_opt_in_changes_cache_key() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.build]
command = "true"

[targets.build.cache]
env = ["CI"]
"#,
    );

    let unset = cached_build_hash(&tmp, None);
    let set = cached_build_hash(&tmp, Some("1"));
    assert_ne!(
        unset, set,
        "opting in with cache.env = [\"CI\"] must change the cache key"
    );
}

/// Sign-in and the aster-srv readers talk to the server and the user's config
/// directory, so they run outside a workspace, legacy spellings included. The
/// rename notices go to stderr so `--json` stdout stays clean.
#[test]
fn srv_commands_run_outside_a_workspace_and_name_renames_on_stderr() {
    let tmp = TempDir::new().unwrap();
    let cases: &[(&[&str], &str, Option<&str>)] = &[
        (&["srv", "login"], "run aster srv login again", None),
        (&["srv", "logout"], "run aster srv logout again", None),
        (&["srv", "whoami"], "run aster srv whoami again", None),
        (
            &["cache", "login"],
            "run aster srv login again",
            Some("`aster cache login` is now `aster srv login`"),
        ),
        (
            &["cache", "logout"],
            "run aster srv logout again",
            Some("`aster cache logout` is now `aster srv logout`"),
        ),
        (&["cache", "stats"], "set ASTER_SRV_URL", None),
        (&["results", "flaky"], "set ASTER_SRV_URL", None),
        (
            &["srv", "flaky"],
            "set ASTER_SRV_URL",
            Some("now `aster results flaky`"),
        ),
    ];
    for (args, error, notice) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(*args)
            .env_remove("ASTER_SRV_URL")
            .env("XDG_CONFIG_HOME", tmp.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {stderr}");
        assert!(
            output.stdout.is_empty(),
            "{args:?}: stdout {:?}",
            output.stdout
        );
        assert!(
            !stderr.contains("Not in an aster workspace"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains(error), "{args:?}: {stderr}");
        match notice {
            Some(notice) => assert!(stderr.contains(notice), "{args:?}: {stderr}"),
            None => assert!(!stderr.contains("is now"), "{args:?}: {stderr}"),
        }
    }
}

/// //services/platform tests read files //services/gateway owns, and
/// platform depends on gateway. Its reads join gateway's test-ci through
/// `selected_by` instead of a reverse `depends_on`, which would be a cycle.
fn selected_by_workspace(tmp: &TempDir) {
    setup_git_repo(tmp);
    write_package_json(
        tmp,
        "services/gateway/package.json",
        r#"{"name":"gateway"}"#,
    );
    write_aster_toml(
        tmp,
        "services/gateway/aster.toml",
        r#"
[targets.build]
command = "true"

[targets.test-ci]
command = "true"
"#,
    );
    write_package_json(
        tmp,
        "services/platform/package.json",
        r#"{"name":"platform"}"#,
    );
    write_aster_toml(
        tmp,
        "services/platform/aster.toml",
        r#"
depends_on = ["//services/gateway"]

[targets.gateway-reads]
command = "true"
selected_by = ["//services/gateway:test-ci"]

[targets.test-ci]
command = "true"
depends_on = ["//self:gateway-reads"]
"#,
    );
    write_aster_toml(
        tmp,
        "aster.toml",
        r#"
[affected.lanes.gateway]
include = ["//services/gateway"]

[affected.lanes.platform]
include = ["//services/platform"]
"#,
    );
    write_fixture(tmp, "services/gateway/index.js", "base");
    write_fixture(tmp, "services/platform/index.js", "base");
    git_commit(tmp, "Initial commit");
}

fn planned(plan: &serde_json::Value) -> Vec<String> {
    let mut addresses: Vec<String> = plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["address"].as_str().unwrap().to_string())
        .collect();
    addresses.sort();
    addresses
}

#[test]
fn selected_by_joins_the_run_of_the_named_target_without_an_edge() {
    let tmp = TempDir::new().unwrap();
    selected_by_workspace(&tmp);
    let lane_plan = |tmp: &TempDir, lane: &str, related: bool| {
        let mut args = vec!["--json", "affected", "test-ci", "--base=HEAD", "--dry-run"];
        if related {
            args.push("--related");
        }
        args.extend(["--lane", lane]);
        aster_json(tmp, &args)
    };

    // No cycle, and the graph reports the relation apart from its edges.
    let graph = aster_json(&tmp, &["--json", "graph"]);
    assert_eq!(
        graph["edges"]["//services/gateway:test-ci"],
        serde_json::json!([])
    );
    assert_eq!(
        graph["selected_by"]["//services/platform:gateway-reads"],
        serde_json::json!(["//services/gateway:test-ci"])
    );

    // A gateway change runs platform's reads in the gateway lane, and says why.
    write_fixture(&tmp, "services/gateway/index.js", "changed");
    for related in [false, true] {
        let plan = lane_plan(&tmp, "gateway", related);
        assert_eq!(
            planned(&plan),
            [
                "//services/gateway:test-ci",
                "//services/platform:gateway-reads"
            ],
            "{plan}"
        );
        let reads = plan["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["address"] == "//services/platform:gateway-reads")
            .unwrap();
        assert_eq!(reads["reason"], "target dependency");
        assert!(
            reads["selection"]
                .to_string()
                .contains("joins the run of //services/gateway:test-ci"),
            "{plan}"
        );
    }
    let rows = aster_json(&tmp, &["affected", "test-ci", "--base=HEAD", "--matrix"]);
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    assert_eq!(rows[0]["lane"], "gateway");

    // A platform change selects only platform: the relation does not run
    // gateway's lane, as a reverse dependency would.
    write_fixture(&tmp, "services/gateway/index.js", "base");
    write_fixture(&tmp, "services/platform/index.js", "changed");
    let rows = aster_json(&tmp, &["affected", "test-ci", "--base=HEAD", "--matrix"]);
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    assert_eq!(rows[0]["lane"], "platform");
    assert_eq!(
        planned(&lane_plan(&tmp, "platform", false)),
        [
            "//services/platform:gateway-reads",
            "//services/platform:test-ci"
        ]
    );

    // Under --dependents a gateway change reaches platform as a dependent;
    // gateway's lane still runs only gateway's test-ci and the reads.
    write_fixture(&tmp, "services/platform/index.js", "base");
    write_fixture(&tmp, "services/gateway/index.js", "changed");
    let plan = aster_json(
        &tmp,
        &[
            "--json",
            "affected",
            "test-ci",
            "--base=HEAD",
            "--dependents",
            "--dry-run",
            "--lane",
            "gateway",
        ],
    );
    assert_eq!(
        planned(&plan),
        [
            "//services/gateway:test-ci",
            "//services/platform:gateway-reads"
        ]
    );
}

#[test]
fn selected_by_must_name_an_existing_target() {
    let tmp = TempDir::new().unwrap();
    selected_by_workspace(&tmp);
    let list = |tmp: &TempDir| {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(["--json", "list"])
            .output()
            .unwrap()
    };
    let platform = |selector: &str| {
        format!("[targets.gateway-reads]\ncommand = \"true\"\nselected_by = [\"{selector}\"]\n")
    };

    write_aster_toml(
        &tmp,
        "services/platform/aster.toml",
        &platform("//services/gateway:lint"),
    );
    let output = list(&tmp);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("is selected_by missing target //services/gateway:lint"),
        "{output:?}"
    );

    // A project alone does not say which of its runs to join.
    write_aster_toml(
        &tmp,
        "services/platform/aster.toml",
        &platform("//services/gateway"),
    );
    let output = list(&tmp);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names a project"),
        "{output:?}"
    );
}

#[test]
fn selected_by_target_runs_in_a_real_affected_run_and_only_when_selected() {
    let tmp = TempDir::new().unwrap();
    selected_by_workspace(&tmp);
    let marker = |name: &str| tmp.path().join(format!("{name}.ran"));
    let touch = |name: &str| format!("touch '{}'", marker(name).display());
    write_aster_toml(
        &tmp,
        "services/gateway/aster.toml",
        &format!(
            "[targets.build]\ncommand = \"true\"\n\n[targets.test-ci]\ncommand = \"{}\"\n",
            touch("gateway-test-ci")
        ),
    );
    write_aster_toml(
        &tmp,
        "services/platform/aster.toml",
        &format!(
            "depends_on = [\"//services/gateway\"]\n\n\
             [targets.gateway-reads]\ncommand = \"{}\"\nselected_by = [\"//services/gateway:test-ci\"]\n\n\
             [targets.test-ci]\ncommand = \"{}\"\ndepends_on = [\"//self:gateway-reads\"]\n",
            touch("gateway-reads"),
            touch("platform-test-ci")
        ),
    );
    git_commit(&tmp, "Write markers from each target");
    let run = |lane: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(["affected", "test-ci", "--base=HEAD", "--lane", lane])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "aster affected --lane {lane} failed: {output:?}"
        );
    };
    let ran = |name: &str| marker(name).exists();

    // A gateway change executes platform's reads in the gateway lane, not
    // platform's own test-ci.
    write_fixture(&tmp, "services/gateway/index.js", "changed");
    run("gateway");
    assert!(ran("gateway-test-ci"));
    assert!(
        ran("gateway-reads"),
        "selected_by target was planned but not executed"
    );
    assert!(!ran("platform-test-ci"));

    // A platform change runs nothing in the gateway lane.
    for name in ["gateway-test-ci", "gateway-reads"] {
        std::fs::remove_file(marker(name)).unwrap();
    }
    write_fixture(&tmp, "services/gateway/index.js", "base");
    write_fixture(&tmp, "services/platform/index.js", "changed");
    run("gateway");
    assert!(!ran("gateway-test-ci") && !ran("gateway-reads"));
}

#[test]
fn selected_by_targets_join_an_aster_run_of_the_named_target() {
    let tmp = TempDir::new().unwrap();
    selected_by_workspace(&tmp);
    write_aster_toml(
        &tmp,
        "services/platform/aster.toml",
        r#"
depends_on = ["//services/gateway"]

[targets.gateway-reads]
command = "touch reads.ran"
selected_by = ["//services/gateway:test-ci"]
cache = { enabled = false }
"#,
    );
    git_commit(&tmp, "Reads leave a marker");
    let marker = tmp.path().join("services/platform/reads.ran");
    let aster = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "aster {args:?}: {output:?}");
    };

    // aster run of the target it joins runs it (affected runs are covered
    // above); --no-deps runs only the named target.
    aster(&["run", "//services/gateway:test-ci", "--no-deps"]);
    assert!(!marker.exists(), "--no-deps ran the joining target");
    aster(&["run", "//services/gateway:test-ci"]);
    assert!(marker.exists(), "aster run did not run the joining target");
    std::fs::remove_file(&marker).unwrap();

    // Another target of the gateway does not.
    aster(&["run", "//services/gateway:build"]);
    assert!(!marker.exists());
}

#[test]
fn target_commands_print_usage_for_help() {
    let tmp = TempDir::new().unwrap();
    assert!(!tmp.path().join(".git").exists());
    assert!(!tmp.path().join("aster.toml").exists());

    for args in [
        &["test", "--help"][..],
        &["build", "-h"][..],
        &["help", "test"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(args)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "aster {args:?} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("--all") && stdout.contains("--no-deps"),
            "aster {args:?} stdout missing target usage:\n{stdout}"
        );
        for flag in ["--dependents", "--lang", "--warnings-as-errors", "--stream"] {
            assert!(
                stdout.contains(flag),
                "aster {args:?} stdout missing {flag}:\n{stdout}"
            );
        }
        assert!(
            !stderr.contains("Not in an aster workspace")
                && !stderr.contains("No projects specified")
                && !stderr.contains("unrecognized subcommand"),
            "aster {args:?} stderr:\n{stderr}"
        );
    }

    let missing = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["help", "nonexistent"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        !missing.status.success(),
        "aster help nonexistent should fail, stdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&missing.stdout)
    );
    assert!(
        stderr.contains("nonexistent"),
        "expected the error to name the unknown command: {stderr}"
    );
    assert!(
        !stderr.contains("Not in an aster workspace"),
        "unknown help target should not require a workspace: {stderr}"
    );
}

#[test]
fn target_help_flags_print_usage_and_run_nothing() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(
        &tmp,
        "app/package.json",
        r#"{"name":"app","scripts":{"build":"touch built.marker","test":"touch tested.marker"}}"#,
    );
    // Help must not load workspace config.
    fs::write(tmp.path().join("aster.toml"), "not = [valid").unwrap();

    let project = tmp.path().join("app");
    let built = project.join("built.marker");
    let tested = project.join("tested.marker");
    let outside = TempDir::new().unwrap();
    let cases: &[(&std::path::Path, &[&str], &str)] = &[
        (&project, &["build", "--help"], "build"),
        (&project, &["build", "-h"], "build"),
        (&project, &["target", "build", "--help"], "build"),
        (&project, &["target", "build", "-h"], "build"),
        (&project, &["test", "--help"], "test"),
        (outside.path(), &["build", "--help"], "build"),
        (outside.path(), &["target", "build", "--help"], "build"),
    ];

    for (cwd, args, usage_target) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(cwd)
            .args(*args)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "aster {args:?} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains(&format!("Usage: aster {usage_target}"))
                && stdout.contains("--all")
                && stdout.contains("--no-deps")
                && stdout.contains("--dependents")
                && stdout.contains("--lang")
                && stdout.contains("--jobs")
                && stdout.contains("--no-cache")
                && stdout.contains("--stream")
                && stdout.contains("--warnings-as-errors"),
            "aster {args:?} stdout missing usage:\n{stdout}"
        );
        assert!(
            !built.exists() && !tested.exists(),
            "aster {args:?} ran a target (built={}, tested={})",
            built.exists(),
            tested.exists()
        );
        assert!(
            !stderr.contains("Not in an aster workspace")
                && !stderr.contains("No projects specified")
                && !stderr.contains("Failed to parse"),
            "aster {args:?} stderr:\n{stderr}"
        );
    }

    // A build without help still runs. Unknown flags are rejected without running.
    fs::remove_file(tmp.path().join("aster.toml")).unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(&project)
        .args(["build", "--no-such-flag", "--no-deps", "--no-cache"])
        .output()
        .unwrap();
    assert_eq!(
        rejected.status.code(),
        Some(2),
        "unknown flag should be rejected: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!built.exists(), "unknown flag must not run the target");
    let ran = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(&project)
        .args(["build", "--no-deps", "--no-cache"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&ran.stdout);
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(
        ran.status.success(),
        "build with an unknown flag should still run\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        built.exists(),
        "build without --help should write the marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!tested.exists(), "build should not have run tests");
}

#[test]
fn target_help_does_not_replace_builtin_help() {
    let bin = env!("CARGO_BIN_EXE_aster");
    let run = |args: &[&str]| {
        let output = Command::new(bin).args(args).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output.status, stdout, stderr)
    };

    for args in [
        &["--help", "target", "build"][..],
        &["-h", "target", "build"],
    ] {
        let (status, stdout, stderr) = run(args);
        assert!(status.success(), "aster {args:?}\n{stdout}\n{stderr}");
        assert!(
            stdout.contains("Usage: aster [OPTIONS]"),
            "root help expected for {args:?}:\n{stdout}"
        );
        assert!(
            !stdout.contains("Usage: aster build"),
            "root help was replaced by target usage for {args:?}:\n{stdout}"
        );
    }

    for args in [&["target", "--help"][..], &["target", "--help", "build"]] {
        let (status, stdout, stderr) = run(args);
        assert!(status.success(), "aster {args:?}\n{stdout}\n{stderr}");
        assert!(
            stdout.contains("Usage: aster target"),
            "built-in target help expected for {args:?}:\n{stdout}"
        );
        assert!(
            !stdout.contains("Usage: aster build"),
            "built-in target help was replaced for {args:?}:\n{stdout}"
        );
    }

    let (status, stdout, stderr) = run(&["check", "--help"]);
    assert!(status.success(), "aster check --help\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("Usage: aster check"),
        "check help changed:\n{stdout}"
    );

    let (status, stdout, stderr) = run(&["--jobs", "--help", "target", "build"]);
    assert!(
        !status.success(),
        "--jobs --help should still fail\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("--jobs"),
        "expected clap to reject --jobs, stderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("Usage: aster build"),
        "invalid --jobs printed target usage:\n{stdout}"
    );
}

#[test]
fn missing_toolchain_program_is_named_in_failure() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "app/package.json", r#"{"name":"app"}"#);
    write_aster_toml(
        &tmp,
        "app/aster.toml",
        r#"
[targets.build]
command = "aster-no-such-tool --version"
cache = { enabled = false }
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["build", "//app", "--no-deps", "--no-cache"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "missing program should fail\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("command not found: aster-no-such-tool"),
        "stderr should name the missing program:\n{stderr}"
    );
}

#[test]
fn init_ignores_aster_state_and_clean_tree_has_no_affected() {
    let tmp = TempDir::new().unwrap();
    setup_git_repo(&tmp);

    let init = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["init"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let init_stdout = String::from_utf8_lossy(&init.stdout);
    assert!(
        init_stdout.contains("Updated .gitignore"),
        "init did not report the gitignore update: {init_stdout}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
        ".aster/\n"
    );

    write_package_json(
        &tmp,
        "apps/api/package.json",
        r#"{"name":"api","scripts":{"build":"mkdir -p dist && echo built > dist/index.js","test":"node --test"}}"#,
    );

    let build = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//apps/api", "--no-deps"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "build failed: {}\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        tmp.path().join(".aster/logs/latest.json").is_file(),
        "build did not write .aster state, so a clean status would be vacuous"
    );
    assert!(tmp.path().join("apps/api/dist/index.js").is_file());

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(status.status.success());
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(
        !porcelain.lines().any(|line| line.contains(".aster")),
        "git status listed .aster state:\n{porcelain}"
    );

    // The tutorial appends build-output ignores before the baseline commit.
    // package-lock.json is committed, matching `git add .` after that step.
    fs::write(
        tmp.path().join(".gitignore"),
        ".aster/\ndist/\nnode_modules/\n",
    )
    .unwrap();
    git_commit(&tmp, "create Aster demo");

    let rerun = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["build", "//apps/api", "--no-deps"])
        .output()
        .unwrap();
    assert!(
        rerun.status.success(),
        "rerun failed: {}\n{}",
        String::from_utf8_lossy(&rerun.stdout),
        String::from_utf8_lossy(&rerun.stderr)
    );
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(
        !porcelain.lines().any(|line| line.contains(".aster")),
        "later run listed .aster state:\n{porcelain}"
    );

    let affected = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["affected", "test", "--base=HEAD", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        affected.status.success(),
        "affected failed: {}\n{}",
        String::from_utf8_lossy(&affected.stdout),
        String::from_utf8_lossy(&affected.stderr)
    );
    let affected_stdout = String::from_utf8_lossy(&affected.stdout);
    assert!(
        affected_stdout.contains("No projects affected"),
        "clean tree still affected projects:\n{affected_stdout}"
    );

    // A second init refuses because aster.toml exists, and must not duplicate
    // the ignore line while doing so.
    let again = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["init"])
        .output()
        .unwrap();
    assert!(!again.status.success(), "second init should refuse");
    assert_eq!(
        fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
        ".aster/\ndist/\nnode_modules/\n"
    );

    // A repository that already ignores .aster does not gain a second line.
    let existing = TempDir::new().unwrap();
    setup_git_repo(&existing);
    fs::write(
        existing.path().join(".gitignore"),
        "node_modules/\n.aster\n",
    )
    .unwrap();
    let init_existing = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(existing.path())
        .args(["init"])
        .output()
        .unwrap();
    assert!(
        init_existing.status.success(),
        "init in an already-ignored repo failed: {}",
        String::from_utf8_lossy(&init_existing.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&init_existing.stdout).contains("Updated .gitignore"),
        "init rewrote a gitignore that already ignored .aster"
    );
    assert_eq!(
        fs::read_to_string(existing.path().join(".gitignore")).unwrap(),
        "node_modules/\n.aster\n"
    );
}

/// `list --json` and `graph --json` bytes do not depend on hash or directory
/// order, and a closed stdout ends `build` without a Rust panic.
#[cfg(unix)]
#[test]
fn json_list_and_graph_are_byte_stable_and_closed_pipe_is_quiet() {
    use std::io::{BufRead, BufReader, Read};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    // Created out of address order so a stable sort is observable.
    for name in ["zeta", "alpha", "mid", "beta", "gamma"] {
        write_package_json(
            &tmp,
            &format!("{name}/package.json"),
            &format!(
                r#"{{"name":"{name}","scripts":{{"build":"echo build-{name}","test":"echo test-{name}"}}}}"#
            ),
        );
        write_aster_toml(
            &tmp,
            &format!("{name}/aster.toml"),
            r#"
[targets.deps]
command = "python3 -c 'open(\"deps-out.txt\",\"w\").write(\"ok\"); print(\"start\"); print(\"x\"*1000000)'"
cache = { enabled = true, outputs = ["deps-out.txt"] }

[targets.build]
command = "python3 -c 'open(\"build-out.txt\",\"w\").write(\"ok\"); print(\"build\")'"
cache = { enabled = true, outputs = ["build-out.txt"] }
"#,
        );
    }
    write_aster_toml(
        &tmp,
        "zeta/aster.toml",
        r#"
[targets.deps]
command = "python3 -c 'open(\"deps-out.txt\",\"w\").write(\"ok\"); print(\"start\"); print(\"x\"*1000000)'"
cache = { enabled = true, outputs = ["deps-out.txt"] }

[targets.build]
command = "python3 -c 'open(\"build-out.txt\",\"w\").write(\"ok\"); print(\"build\")'"
cache = { enabled = true, outputs = ["build-out.txt"] }

[targets.disposition]
command = "cat /proc/self/status"
cache = { enabled = false }
"#,
    );

    let aster = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aster"));
        command.current_dir(tmp.path());
        command
    };

    let mut list_runs = Vec::new();
    let mut graph_runs = Vec::new();
    for _ in 0..10 {
        let list = aster().args(["list", "--json"]).output().unwrap();
        assert!(
            list.status.success(),
            "list failed: {}",
            String::from_utf8_lossy(&list.stderr)
        );
        list_runs.push(list.stdout);
        let graph = aster().args(["graph", "--json"]).output().unwrap();
        assert!(
            graph.status.success(),
            "graph failed: {}",
            String::from_utf8_lossy(&graph.stderr)
        );
        graph_runs.push(graph.stdout);
    }
    for run in &list_runs[1..] {
        assert_eq!(run, &list_runs[0], "list --json bytes changed between runs");
    }
    for run in &graph_runs[1..] {
        assert_eq!(
            run, &graph_runs[0],
            "graph --json bytes changed between runs"
        );
    }

    let projects: serde_json::Value = serde_json::from_slice(&list_runs[0]).unwrap();
    let projects = projects.as_array().expect("list --json is an array");
    assert!(
        projects.len() >= 4,
        "expected at least four projects, got {}",
        projects.len()
    );
    let addresses: Vec<&str> = projects
        .iter()
        .map(|project| project["address"].as_str().unwrap())
        .collect();
    let mut sorted_addresses = addresses.clone();
    sorted_addresses.sort();
    assert_eq!(
        addresses, sorted_addresses,
        "projects are not sorted by address"
    );
    assert_eq!(
        addresses,
        vec!["//alpha", "//beta", "//gamma", "//mid", "//zeta"]
    );
    for project in projects {
        let targets = project["targets"].as_object().expect("targets object");
        let keys: Vec<&String> = targets.keys().collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys, "target keys are not sorted: {keys:?}");
        assert!(
            keys.iter().any(|key| key.as_str() == "build")
                && keys.iter().any(|key| key.as_str() == "test"),
            "expected build and test targets: {keys:?}"
        );
    }

    let human = aster().args(["list"]).output().unwrap();
    assert!(human.status.success(), "{human:?}");
    let human_again = aster().args(["list"]).output().unwrap();
    assert_eq!(
        human.stdout, human_again.stdout,
        "human list output changed between runs"
    );
    let human_graph = aster().args(["graph"]).output().unwrap();
    assert!(human_graph.status.success(), "{human_graph:?}");
    let human_graph_again = aster().args(["graph"]).output().unwrap();
    assert_eq!(human_graph.stdout, human_graph_again.stdout);

    let graph: serde_json::Value = serde_json::from_slice(&graph_runs[0]).unwrap();
    let nodes = graph["nodes"].as_array().expect("nodes");
    let node_names: Vec<&str> = nodes.iter().map(|node| node.as_str().unwrap()).collect();
    let mut sorted_nodes = node_names.clone();
    sorted_nodes.sort();
    assert_eq!(node_names, sorted_nodes, "graph nodes are not sorted");
    assert!(node_names.len() >= 4, "expected several graph nodes");
    let edges = graph["edges"].as_object().expect("edges");
    let edge_keys: Vec<&String> = edges.keys().collect();
    let mut sorted_edge_keys = edge_keys.clone();
    sorted_edge_keys.sort();
    assert_eq!(edge_keys, sorted_edge_keys, "edge keys are not sorted");
    for (address, dependencies) in edges {
        let dependencies = dependencies.as_array().expect("edge list");
        let names: Vec<&str> = dependencies
            .iter()
            .map(|dependency| dependency.as_str().unwrap())
            .collect();
        let mut sorted_names = names.clone();
        sorted_names.sort();
        assert_eq!(
            names, sorted_names,
            "edge list for {address} is not sorted: {names:?}"
        );
    }

    #[cfg(target_os = "linux")]
    {
        let disposition = aster()
            .args([
                "run",
                "//zeta:disposition",
                "--no-deps",
                "--json",
                "--no-cache",
            ])
            .output()
            .unwrap();
        assert!(
            disposition.status.success(),
            "disposition probe failed: {}",
            String::from_utf8_lossy(&disposition.stderr)
        );
        let parsed: serde_json::Value = serde_json::from_slice(&disposition.stdout).unwrap();
        let status = parsed["results"][0]["output"]
            .as_str()
            .expect("child status output");
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap_or_else(|| panic!("missing {name} in {status}"))
                .trim()
                .to_string()
        };
        let bit_set = |mask: &str| {
            let value = u64::from_str_radix(mask, 16).unwrap();
            // SIGPIPE is signal 13, bit 12 of the kernel mask.
            value & (1 << 12) != 0
        };
        assert!(
            !bit_set(&field("SigIgn:")) && !bit_set(&field("SigCgt:")),
            "spawned target did not keep the default SIGPIPE disposition:\n{status}"
        );
    }

    // Cache is explicit so the second `build --all` replays the stored log
    // on stdout. That log is larger than a pipe buffer, so closing after the
    // first line interrupts the write. A summary-only write finishes before
    // the reader can close the pipe.
    let warm = aster().args(["build", "--all"]).output().unwrap();
    assert!(
        warm.status.success(),
        "warm build failed: {}\n{}",
        String::from_utf8_lossy(&warm.stdout),
        String::from_utf8_lossy(&warm.stderr)
    );

    let mut child = aster()
        .args(["build", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let read = reader.read_line(&mut line);
        drop(reader);
        let _ = sender.send((read, line));
    });
    let (read, line) = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("aster build produced no stdout line before the pipe was closed");
    assert!(
        read.unwrap() > 0,
        "closed the pipe before any stdout line: {line:?}"
    );
    let status = child.wait().unwrap();
    let stderr = String::from_utf8_lossy(&stderr_thread.join().unwrap()).into_owned();
    assert!(
        !stderr.contains("panicked"),
        "closed pipe panicked: status={status:?} stderr={stderr}"
    );
    assert_ne!(
        status.code(),
        Some(101),
        "closed pipe exited with a Rust panic code: {stderr}"
    );
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGPIPE),
        "closed pipe should die on SIGPIPE, got {status:?}; stderr={stderr}"
    );
}
