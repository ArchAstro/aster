#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("condition was not satisfied within {timeout:?}");
}

fn occurrences(path: &Path, needle: &str) -> usize {
    fs::read_to_string(path)
        .unwrap_or_default()
        .matches(needle)
        .count()
}

fn control_request(port: u16, request: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

fn wait_for_control_token(port: u16, process_id: u32) -> (std::path::PathBuf, String) {
    let prefix = format!("aster-dev-{port}-{process_id}-");
    let mut found = None;
    wait_until(Duration::from_secs(5), || {
        found = fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".token"))
            });
        found.is_some()
    });
    let path = found.unwrap();
    let token = fs::read_to_string(&path).unwrap();
    (path, token)
}

#[test]
fn dev_supervises_targets_runs_prerequisites_and_restarts_on_dependency_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let control_port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("app")).unwrap();
    fs::create_dir_all(root.join("lib/src")).unwrap();
    fs::create_dir_all(root.join("lib/generated")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(root.join("lib/package.json"), r#"{"name":"lib"}"#).unwrap();
    fs::write(root.join("lib/src/input.js"), "first").unwrap();

    // Configuration boundary: a service maps to one stream target, and ordinary
    // target dependencies describe both preparation and the watched library.
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev]
control_port = "control"

[watch]
suppress_paths = ["lib/src/suppressed.js"]

[dev.ports.http]
default = {port}

[dev.ports.control]
default = {control_port}

[dev.services.web]
target = "//app:dev"
port = "http"
inherit_env = ["ASTER_ALLOWED_VALUE"]
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.prepare]
command = "sh -c 'count=$(grep -c PREPARE ../events.log 2>/dev/null || true); echo PREPARE >> ../events.log; if [ \"$count\" -ge 1 ]; then sleep 1; echo generated > ../lib/src/suppressed.js; fi'"
depends_on = ["//lib:build"]

[targets.dev]
command = "sh -c 'if [ -n \"${ASTER_AMBIENT_SECRET:-}\" ]; then echo AMBIENT_SECRET_LEAKED >> ../events.log; fi; echo INHERITED:$ASTER_ALLOWED_VALUE >> ../events.log; echo START:$ASTER_SERVICE_PORT >> ../events.log; python3 -m http.server {port} & server=$!; echo $server > ../child.pid; wait $server'"
depends_on = ["//self:prepare"]
stream = true
"#,
    )
    .unwrap();
    fs::write(
        root.join("lib/aster.toml"),
        r#"
[targets.build]
command = "sh -c 'echo BUILD >> ../events.log'"
"#,
    )
    .unwrap();

    // Process boundary: launch the public CLI, which starts a real prerequisite
    // process and a real HTTP child in its own process group.
    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["dev", "--no-ui"])
        .current_dir(root)
        .env("ASTER_AMBIENT_SECRET", "must-not-reach-service")
        .env("ASTER_ALLOWED_VALUE", "explicitly-allowed")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let events = root.join("events.log");
    wait_until(Duration::from_secs(12), || {
        occurrences(&events, "PREPARE") >= 1
            && occurrences(&events, &format!("START:{port}")) >= 1
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert_eq!(occurrences(&events, "AMBIENT_SECRET_LEAKED"), 0);
    assert!(fs::read_to_string(&events)
        .unwrap()
        .contains("INHERITED:explicitly-allowed"));
    let status = control_request(control_port, r#"{"command":"status"}"#);
    assert_eq!(status["ok"], true);
    assert_eq!(status["services"]["web"], "running");
    let unauthorized = control_request(control_port, r#"{"command":"restart_all"}"#);
    assert_eq!(unauthorized["ok"], false);
    assert_eq!(unauthorized["error"], "valid control token required");

    // The first event after startup is still eligible once the suppression
    // window expires, even when its path matches suppress_paths.
    thread::sleep(Duration::from_secs(1));
    fs::write(root.join("lib/src/suppressed.js"), "manual").unwrap();
    wait_until(Duration::from_secs(10), || {
        occurrences(&events, "PREPARE") >= 2 && TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    thread::sleep(Duration::from_secs(2));

    // Watch boundary: mutate the transitive library dependency, not the service
    // directory. Mutate it again while the deliberately slow prerequisite is
    // running; that genuine source event must survive the restart cooldown.
    fs::write(root.join("lib/src/input.js"), "second").unwrap();
    wait_until(Duration::from_secs(10), || {
        occurrences(&events, "PREPARE") >= 3
    });
    thread::sleep(Duration::from_millis(250));
    fs::write(root.join("lib/src/input.js"), "third").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline
        && (occurrences(&events, "PREPARE") < 4 || TcpStream::connect(("127.0.0.1", port)).is_err())
    {
        thread::sleep(Duration::from_millis(100));
    }
    if occurrences(&events, "PREPARE") < 4 || TcpStream::connect(("127.0.0.1", port)).is_err() {
        unsafe {
            libc::kill(aster.id() as i32, libc::SIGTERM);
        }
        let output = aster.wait_with_output().unwrap();
        panic!(
            "restart did not settle:\n{}\nstdout:\n{}\nstderr:\n{}",
            fs::read_to_string(&events).unwrap_or_default(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    thread::sleep(Duration::from_secs(2));
    assert_eq!(occurrences(&events, "PREPARE"), 4);

    // Shutdown boundary: SIGTERM the launcher and observe conventional status
    // plus complete process-group cleanup of the HTTP descendant.
    let child_pid: i32 = fs::read_to_string(root.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _partial_control_request = TcpStream::connect(("127.0.0.1", control_port)).unwrap();
    thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::kill(aster.id() as i32, libc::SIGTERM);
    }
    let status = aster.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    wait_until(Duration::from_secs(5), || unsafe {
        libc::kill(child_pid, 0) == -1
    });
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn dev_restores_through_normal_shutdown_when_every_service_fails_to_start() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let control_port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev]
control_port = "control"

[dev.ports.control]
default = {control_port}

[dev.services.broken]
target = "//app:dev"

[dev.services.crashy]
target = "//app:crash"
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.dev]
command = "this-command-does-not-exist"
depends_on = ["//self:prepare"]
stream = true

[targets.prepare]
command = "sh -c 'echo PRESTEP_DIAGNOSTIC >&2; exit 7'"

[targets.crash]
command = "sh -c 'touch ../crash-ran; printf PART; printf IAL; printf \"\\377\"; echo FINAL >&2; exit 7'"
stream = true
"#,
    )
    .unwrap();

    // No supervised child is ever registered. SIGTERM must still take the
    // graceful dev-loop path so an interactive caller can restore its terminal.
    let aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["dev", "--no-ui"])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", control_port)).is_ok() && root.join("crash-ran").exists()
    });
    let status = control_request(control_port, r#"{"command":"status"}"#);
    assert_eq!(status["services"]["broken"], "stopped");
    assert_eq!(status["services"]["crashy"], "stopped");
    unsafe {
        libc::kill(aster.id() as i32, libc::SIGTERM);
    }
    let output = aster.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PRESTEP_DIAGNOSTIC"));
    assert!(stdout.contains("prerequisite failed"));
    assert!(stdout.contains("PARTIAL"));
    assert!(stdout.contains("FINAL"));
}

#[test]
fn dev_does_not_start_a_service_after_shutdown_interrupts_its_prerequisite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("aster.toml"),
        r#"
[dev.services.web]
target = "//app:dev"
"#,
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.prepare]
command = "sh -c 'trap \"exit 0\" TERM; echo PREPARE_STARTED >> ../events.log; sleep 30'"
cache = { enabled = true, include = ["package.json"] }

[targets.dev]
command = "sh -c 'echo SERVICE_STARTED >> ../events.log; sleep 30'"
depends_on = ["//self:prepare"]
stream = true
"#,
    )
    .unwrap();

    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["dev", "--no-ui"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let events = root.join("events.log");
    wait_until(Duration::from_secs(5), || {
        occurrences(&events, "PREPARE_STARTED") == 1
    });
    unsafe {
        libc::kill(aster.id() as i32, libc::SIGTERM);
    }
    let status = aster.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    assert_eq!(occurrences(&events, "SERVICE_STARTED"), 0);
    assert!(!fs::read_to_string(root.join(".aster/cache.json"))
        .unwrap_or_default()
        .contains("//app:prepare"));
}

#[test]
fn authenticated_control_shutdown_interrupts_an_in_progress_prerequisite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let control_port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev]
control_port = "control"

[dev.ports.control]
default = {control_port}

[dev.services.web]
target = "//app:dev"
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.prepare]
command = "sh -c 'trap \"exit 0\" TERM; echo PREPARE_STARTED >> ../events.log; sleep 30'"

[targets.later]
command = "sh -c 'echo LATER_SIDE_EFFECT >> ../events.log'"
depends_on = ["//self:prepare"]

[targets.dev]
command = "sh -c 'echo SERVICE_STARTED >> ../events.log; sleep 30'"
depends_on = ["//self:later"]
stream = true
"#,
    )
    .unwrap();

    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["dev", "--no-ui"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let events = root.join("events.log");
    wait_until(Duration::from_secs(5), || {
        occurrences(&events, "PREPARE_STARTED") == 1
    });
    let (token_path, token) = wait_for_control_token(control_port, aster.id());
    let request = serde_json::json!({"command": "shutdown", "token": token}).to_string();
    let response = control_request(control_port, &request);
    assert_eq!(response["ok"], true);
    let status = aster.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(occurrences(&events, "SERVICE_STARTED"), 0);
    assert_eq!(occurrences(&events, "LATER_SIDE_EFFECT"), 0);
    assert!(!token_path.exists());
}
