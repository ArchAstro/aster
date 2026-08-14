//! Integration tests for `aster watch`.
//!
//! These tests exercise the full binary: they scaffold a tempfile workspace,
//! spawn `aster watch`, touch files, and assert that the watcher's stdout
//! contains the expected rebuild banners. They depend on fs-event timing so
//! they wait a few seconds and are kept simple.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn setup_workspace(tmp: &TempDir) {
    fs::create_dir(tmp.path().join(".git")).unwrap();
}

fn write(tmp: &TempDir, path: &str, content: &str) {
    let full = tmp.path().join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn aster_bin() -> &'static str {
    env!("CARGO_BIN_EXE_aster")
}

/// Run a command, collect lines asynchronously into an mpsc channel, and return
/// the child + receiver so the test can wait for specific markers and then
/// kill the process.
fn spawn_capturing(mut cmd: Command) -> (std::process::Child, mpsc::Receiver<String>) {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Aster's shutdown handler terminates its process group so streamed target
    // descendants cannot be orphaned. Give the test child its own group; CI
    // shells run without job control and would otherwise share the cargo test
    // process group, causing SIGTERM cleanup to cancel the entire job.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().expect("spawn aster watch");
    #[cfg(unix)]
    assert_eq!(
        unsafe { libc::getpgid(child.id() as i32) },
        child.id() as i32,
        "watch test child must own its process group"
    );

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();

    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx2.send(line);
        }
    });

    (child, rx)
}

fn wait_for_line(
    rx: &mpsc::Receiver<String>,
    substring: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(line) => {
                if line.contains(substring) {
                    return Some(line);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
    None
}

fn nodejs_package(name: &str) -> String {
    format!(
        r#"{{
  "name": "{name}",
  "version": "1.0.0",
  "scripts": {{
    "build": "echo {name} built"
  }}
}}"#
    )
}

fn nodejs_package_with_dep(name: &str, dep_path: &str) -> String {
    format!(
        r#"{{
  "name": "{name}",
  "version": "1.0.0",
  "scripts": {{
    "build": "echo {name} built"
  }},
  "dependencies": {{
    "core": "file:{dep_path}"
  }}
}}"#
    )
}

fn kill_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[test]
fn watch_rejects_missing_target() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write(&tmp, "services/api/package.json", &nodejs_package("api"));

    let output = Command::new(aster_bin())
        .current_dir(tmp.path())
        .arg("watch")
        .arg("//services/api:nonexistent")
        .output()
        .expect("run aster watch");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("target not found") || stderr.contains("//services/api:nonexistent"),
        "expected missing-target error, got: {stderr}"
    );
}

#[test]
fn watch_rejects_glob_selector() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write(&tmp, "services/api/package.json", &nodejs_package("api"));

    let output = Command::new(aster_bin())
        .current_dir(tmp.path())
        .arg("watch")
        .arg("//services/...")
        .output()
        .expect("run aster watch");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("glob selectors are not yet supported"),
        "expected glob rejection, got: {stderr}"
    );
}

#[test]
fn watch_expands_bare_project_to_default_target() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write(&tmp, "services/api/package.json", &nodejs_package("api"));

    // Bare project address with default --target=build should be accepted.
    // We exit immediately via --no-initial and SIGTERM, just verify startup.
    let mut cmd = Command::new(aster_bin());
    cmd.current_dir(tmp.path())
        .arg("watch")
        .arg("//services/api")
        .arg("--no-initial");

    let (mut child, rx) = spawn_capturing(cmd);
    let banner = wait_for_line(&rx, "watching", Duration::from_secs(5));
    kill_child(&mut child);

    assert!(banner.is_some(), "expected watch startup banner");
}

/// Wait `wait` seconds and assert the channel has NOT received any line
/// containing `substring`. Returns the lines observed for diagnostics.
fn assert_no_line_containing(
    rx: &mpsc::Receiver<String>,
    substring: &str,
    wait: Duration,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + wait;
    let mut observed = Vec::new();
    while std::time::Instant::now() < deadline {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(line) => {
                assert!(
                    !line.contains(substring),
                    "unexpected {substring:?} in watch output: {line}"
                );
                observed.push(line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    observed
}

/// Discard platform watcher events that predate the startup banner.
///
/// macOS FSEvents may report fixture files created immediately before the
/// watcher registered. Those lines must not be attributed to the later edit
/// each test is trying to classify.
fn drain_startup_events(rx: &mpsc::Receiver<String>) {
    let started = std::time::Instant::now();
    let minimum_deadline = started + Duration::from_secs(2);
    let maximum_deadline = started + Duration::from_secs(8);
    let quiet_period = Duration::from_millis(500);
    let mut quiet_deadline = minimum_deadline;

    while std::time::Instant::now() < maximum_deadline {
        let now = std::time::Instant::now();
        let wait_until = quiet_deadline.min(maximum_deadline);
        let remaining = wait_until.checked_duration_since(now).unwrap_or_default();
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(_) => {
                quiet_deadline = (std::time::Instant::now() + quiet_period).max(minimum_deadline);
            }
            Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() >= quiet_deadline => {
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[test]
fn watch_ignores_non_input_file_changes() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write(&tmp, "libs/core/package.json", &nodejs_package("core"));
    write(&tmp, "libs/core/src/index.ts", "export const x = 1;\n");
    write(&tmp, "libs/core/README.md", "# core\n");

    let mut cmd = Command::new(aster_bin());
    cmd.current_dir(tmp.path())
        .arg("watch")
        .arg("//libs/core:build")
        .arg("--no-initial")
        .arg("--debounce")
        .arg("100ms");

    let (mut child, rx) = spawn_capturing(cmd);
    assert!(wait_for_line(&rx, "watching", Duration::from_secs(5)).is_some());

    drain_startup_events(&rx);

    // Touch a file the build target's cache_inputs does NOT cover. The watcher
    // must not emit a change banner or rebuild.
    fs::write(tmp.path().join("libs/core/README.md"), "# core edited\n").unwrap();

    // "change:" is the banner printed when a rebuild is about to fire. Wait
    // briefly and assert it's not in the output.
    let observed = assert_no_line_containing(&rx, "change:", Duration::from_secs(2));
    kill_child(&mut child);

    assert!(
        !observed.iter().any(|l| l.contains("change:")),
        "README.md edit should not trigger watch cycle"
    );
}

#[test]
fn watch_ignores_dotgit_changes() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write(&tmp, "libs/core/package.json", &nodejs_package("core"));
    write(&tmp, "libs/core/src/index.ts", "export const x = 1;\n");

    let mut cmd = Command::new(aster_bin());
    cmd.current_dir(tmp.path())
        .arg("watch")
        .arg("//libs/core:build")
        .arg("--no-initial")
        .arg("--debounce")
        .arg("100ms");

    let (mut child, rx) = spawn_capturing(cmd);
    assert!(wait_for_line(&rx, "watching", Duration::from_secs(5)).is_some());
    drain_startup_events(&rx);

    // Simulate a git internal write.
    fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let observed = assert_no_line_containing(&rx, "change:", Duration::from_secs(2));
    kill_child(&mut child);

    assert!(!observed.iter().any(|l| l.contains("change:")));
}

#[test]
fn watch_ignores_node_modules_changes() {
    // node_modules is in the built-in ignore defaults — a write there during
    // e.g. `npm install` must not trigger any rebuild.
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write(&tmp, "libs/core/package.json", &nodejs_package("core"));
    write(&tmp, "libs/core/src/index.ts", "export const x = 1;\n");
    let mut cmd = Command::new(aster_bin());
    cmd.current_dir(tmp.path())
        .arg("watch")
        .arg("//libs/core:build")
        .arg("--no-initial")
        .arg("--debounce")
        .arg("100ms");

    let (mut child, rx) = spawn_capturing(cmd);
    assert!(wait_for_line(&rx, "watching", Duration::from_secs(5)).is_some());
    drain_startup_events(&rx);

    // Model a dependency install creating node_modules after the watcher has
    // started. The entire creation must remain ignored.
    write(
        &tmp,
        "libs/core/node_modules/foo/index.js",
        "module.exports = 1;\n",
    );

    let observed = assert_no_line_containing(&rx, "change:", Duration::from_secs(2));
    kill_child(&mut child);

    assert!(!observed.iter().any(|l| l.contains("change:")));
}

#[test]
fn watch_reruns_dependent_target_on_dep_source_change() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write(&tmp, "libs/core/package.json", &nodejs_package("core"));
    write(&tmp, "libs/core/src/index.ts", "export const x = 1;\n");

    write(
        &tmp,
        "services/api/package.json",
        &nodejs_package_with_dep("api", "../../libs/core"),
    );
    write(&tmp, "services/api/src/main.ts", "console.log(1);\n");

    let mut cmd = Command::new(aster_bin());
    cmd.current_dir(tmp.path())
        .arg("watch")
        .arg("//services/api:build")
        .arg("--no-initial")
        .arg("--debounce")
        .arg("100ms");

    let (mut child, rx) = spawn_capturing(cmd);
    assert!(wait_for_line(&rx, "watching", Duration::from_secs(5)).is_some());

    drain_startup_events(&rx);
    // Touch the core source file.
    fs::write(
        tmp.path().join("libs/core/src/index.ts"),
        "export const x = 2;\n",
    )
    .unwrap();

    // Expect the change banner AND the start of libs/core:build.
    let change = wait_for_line(&rx, "change:", Duration::from_secs(8));
    let core_build = wait_for_line(&rx, "//libs/core:build", Duration::from_secs(8));
    let api_build = wait_for_line(&rx, "//services/api:build", Duration::from_secs(8));

    kill_child(&mut child);

    assert!(change.is_some(), "expected change banner");
    assert!(core_build.is_some(), "expected //libs/core:build rebuild");
    assert!(api_build.is_some(), "expected //services/api:build rebuild");
}
