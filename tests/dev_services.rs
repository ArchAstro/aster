#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

fn condition_met(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    condition()
}

fn wait_until(timeout: Duration, condition: impl FnMut() -> bool) {
    assert!(
        condition_met(timeout, condition),
        "condition was not satisfied within {timeout:?}"
    );
}

/// These tests launch real supervisors and listeners. Running them in parallel
/// creates released-port races and can starve process startup on macOS CI.
fn service_process_test() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn occurrences(path: &Path, needle: &str) -> usize {
    fs::read_to_string(path)
        .unwrap_or_default()
        .matches(needle)
        .count()
}

fn allocation_manifest_count(lease_dir: &Path) -> usize {
    fs::read_dir(lease_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("allocation-") && name.ends_with(".json"))
        })
        .count()
}

fn process_is_running(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == -1 {
        return false;
    }
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    !state.trim().is_empty() && !state.trim_start().starts_with('Z')
}

fn fail_with_process_diagnostics(
    aster: &mut std::process::Child,
    events: &Path,
    stdout: &Path,
    stderr: &Path,
    context: &str,
) -> ! {
    unsafe {
        libc::kill(aster.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut status = None;
    while Instant::now() < deadline {
        status = aster.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if status.is_none() {
        aster.kill().unwrap();
        status = Some(aster.wait().unwrap());
    }
    panic!(
        "{context}:\nstatus: {:?}\nevents:\n{}\nstdout:\n{}\nstderr:\n{}",
        status.unwrap(),
        fs::read_to_string(events).unwrap_or_default(),
        fs::read_to_string(stdout).unwrap_or_default(),
        fs::read_to_string(stderr).unwrap_or_default(),
    );
}

fn control_request(port: u16, request: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

fn wait_for_control_token(port: u16, process_id: u32) -> (std::path::PathBuf, String) {
    let prefix = format!("aster-services-{port}-{process_id}-");
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

fn reserve_consecutive_dynamic_bundles() -> (u16, u16) {
    for start in 30000u16..60000u16 {
        let Some(derived_start) = start.checked_add(1000) else {
            break;
        };
        let Some(derived_next) = derived_start.checked_add(1) else {
            break;
        };
        let listeners = [start, start + 1, derived_start, derived_next]
            .into_iter()
            .map(|port| TcpListener::bind(("127.0.0.1", port)))
            .collect::<Result<Vec<_>, _>>();
        if let Ok(listeners) = listeners {
            drop(listeners);
            return (start, derived_start);
        }
    }
    panic!("could not find two consecutive free dynamic port bundles");
}

fn terminate_aster(child: &mut std::process::Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
}

#[test]
fn services_kill_ports_previews_then_clears_configured_listener() {
    let _serial = service_process_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut listener = Command::new("python3")
        .args([
            "-c",
            "import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',int(__import__('sys').argv[1]))); s.listen(); time.sleep(60)",
            &port.to_string(),
        ])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });

    // Explicit numeric cleanup works without an aster.toml or .git workspace.
    let outside_workspace = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports", &port.to_string(), "--dry-run"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(outside_workspace.status.success());
    assert!(String::from_utf8_lossy(&outside_workspace.stdout).contains("Would terminate"));
    assert!(listener.try_wait().unwrap().is_none());

    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("aster.toml"),
        format!("[dev.ports.web]\ndefault = {port}\n"),
    )
    .unwrap();

    let preview = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports", "web", "--dry-run"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("Would terminate"));
    assert!(listener.try_wait().unwrap().is_none());

    let cleanup = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports"])
        .current_dir(root)
        .output()
        .unwrap();
    if !cleanup.status.success() {
        let _ = listener.kill();
        let _ = listener.wait();
    }
    assert!(
        cleanup.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&cleanup.stdout),
        String::from_utf8_lossy(&cleanup.stderr)
    );
    wait_until(Duration::from_secs(3), || {
        listener.try_wait().unwrap().is_some()
    });
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(String::from_utf8_lossy(&cleanup.stdout).contains("Cleared"));
}

#[test]
fn dynamic_port_bundles_are_distinct_propagated_and_released() {
    let _serial = service_process_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let lease_dir = root.join("leases");
    let (start, derived_start) = reserve_consecutive_dynamic_bundles();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(root.join("service.env"), "DEPENDENT_PORT=wrong\n").unwrap();
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev.ports.http]
allocation = "dynamic"
range = [{start}, {}]
preferred = {start}

[dev.ports.dependent]
default = {derived_start}
offset_from = "http"
offset_base = {start}

[dev.services.web]
target = "//app:dev"
port = "http"
env_files = ["service.env"]
port_env = {{ DEPENDENT_PORT = "dependent" }}
"#,
            start + 1
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.dev]
command = "sh -c 'echo $ASTER_SERVICE_PORT:$DEPENDENT_PORT >> ../events.log; python3 -m http.server {port}'"
stream = true
"#,
    )
    .unwrap();

    let launch = || {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(["services", "up", "--no-ui", "--no-watch"])
            .current_dir(root)
            .env("ASTER_PORT_LEASE_DIR", &lease_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };

    let events = root.join("events.log");
    let mut first = launch();
    wait_until(Duration::from_secs(20), || {
        fs::read_to_string(&events)
            .unwrap_or_default()
            .contains(&format!("{start}:{derived_start}"))
            && TcpStream::connect(("127.0.0.1", start)).is_ok()
    });
    assert_eq!(allocation_manifest_count(&lease_dir), 1);

    let mut second = launch();
    wait_until(Duration::from_secs(20), || {
        fs::read_to_string(&events)
            .unwrap_or_default()
            .contains(&format!("{}:{}", start + 1, derived_start + 1))
            && TcpStream::connect(("127.0.0.1", start + 1)).is_ok()
    });
    assert_eq!(allocation_manifest_count(&lease_dir), 2);
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());

    let json_ports = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["--json", "services", "ports"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(json_ports.status.success());
    let report: serde_json::Value = serde_json::from_slice(&json_ports.stdout).unwrap();
    assert_eq!(
        report["workspace"],
        root.canonicalize().unwrap().to_string_lossy().as_ref()
    );
    let instances = report["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2);
    for expected_port in [start, start + 1] {
        let instance = instances
            .iter()
            .find(|instance| instance["ports"]["http"] == expected_port)
            .unwrap();
        assert_eq!(instance["status"], "active");
        assert_eq!(
            instance["ports"]["dependent"],
            derived_start + expected_port - start
        );
        assert_eq!(instance["services"][0]["name"], "web");
        assert_eq!(instance["services"][0]["port_name"], "http");
        assert_eq!(instance["services"][0]["port"], expected_port);
    }

    let human_ports = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "ports"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(human_ports.status.success());
    let human_ports = String::from_utf8_lossy(&human_ports.stdout);
    assert!(human_ports.contains("SERVICE"));
    assert!(human_ports.contains("web"));
    assert!(human_ports.contains("dependent"));
    assert!(human_ports.contains(&start.to_string()));
    assert!(human_ports.contains(&(start + 1).to_string()));

    terminate_aster(&mut first);
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", start)).is_err()
            && allocation_manifest_count(&lease_dir) == 1
    });

    let previous = occurrences(&events, &format!("{start}:{derived_start}"));
    let mut third = launch();
    wait_until(Duration::from_secs(20), || {
        occurrences(&events, &format!("{start}:{derived_start}")) > previous
            && TcpStream::connect(("127.0.0.1", start)).is_ok()
    });

    terminate_aster(&mut third);
    terminate_aster(&mut second);
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", start)).is_err()
            && TcpStream::connect(("127.0.0.1", start + 1)).is_err()
            && allocation_manifest_count(&lease_dir) == 0
    });

    let empty_ports = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "ports", "--json"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(empty_ports.status.success());
    let report: serde_json::Value = serde_json::from_slice(&empty_ports.stdout).unwrap();
    assert!(report["instances"].as_array().unwrap().is_empty());
}

#[test]
fn ports_reports_static_and_portless_services_from_the_running_instance() {
    let _serial = service_process_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let lease_dir = root.join("leases");
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);

    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev.ports.http]
default = {port}

[dev.services.web]
target = "//app:web"
port = "http"

[dev.services.worker]
target = "//app:worker"
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.web]
command = "python3 -m http.server {port}"
stream = true

[targets.worker]
command = "sleep 30"
stream = true
"#,
    )
    .unwrap();

    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "--no-ui", "--no-watch"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(20), || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "ports", "--json"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let instance = &report["instances"][0];
    assert_eq!(instance["status"], "active");
    assert_eq!(instance["ports"]["http"], port);
    let services = instance["services"].as_array().unwrap();
    let web = services
        .iter()
        .find(|service| service["name"] == "web")
        .unwrap();
    assert_eq!(web["port_name"], "http");
    assert_eq!(web["port"], port);
    let worker = services
        .iter()
        .find(|service| service["name"] == "worker")
        .unwrap();
    assert!(worker["port_name"].is_null());
    assert!(worker["port"].is_null());

    let human = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "ports"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(human.status.success());
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.lines().any(|line| {
        line.contains("web") && line.contains("http") && line.contains(&port.to_string())
    }));
    assert!(human
        .lines()
        .any(|line| line.contains("worker") && line.contains("active")));

    terminate_aster(&mut supervisor);
}

#[test]
fn kill_ports_recovers_dynamic_listener_after_supervisor_crash() {
    let _serial = service_process_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let lease_dir = root.join("leases");
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);

    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev.ports.http]
allocation = "dynamic"
range = [{port}, {port}]

[dev.services.web]
target = "//app:dev"
port = "http"
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.dev]
command = "python3 -m http.server {port}"
stream = true
"#,
    )
    .unwrap();

    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "--no-ui", "--no-watch"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(20), || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
            && allocation_manifest_count(&lease_dir) == 1
    });

    let active_preview = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports", "http", "--dry-run"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(active_preview.status.success());
    assert!(String::from_utf8_lossy(&active_preview.stdout).contains("Would terminate"));
    assert!(supervisor.try_wait().unwrap().is_none());

    // SIGKILL bypasses PortLease::drop, reproducing a supervisor crash while
    // its independently running service process remains alive.
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert_eq!(allocation_manifest_count(&lease_dir), 1);

    let orphaned_ports = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["--json", "services", "ports"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(orphaned_ports.status.success());
    let report: serde_json::Value = serde_json::from_slice(&orphaned_ports.stdout).unwrap();
    assert_eq!(report["instances"].as_array().unwrap().len(), 1);
    assert_eq!(report["instances"][0]["status"], "orphaned");
    assert_eq!(report["instances"][0]["ports"]["http"], port);
    assert_eq!(report["instances"][0]["services"][0]["name"], "web");

    let other_workspace = temp.path().join("other-worktree");
    fs::create_dir(&other_workspace).unwrap();
    fs::create_dir(other_workspace.join(".git")).unwrap();
    fs::write(
        other_workspace.join("aster.toml"),
        format!("[dev.ports.http]\nallocation = \"dynamic\"\nrange = [{port}, {port}]\n"),
    )
    .unwrap();
    let isolated = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports", "http", "--dry-run"])
        .current_dir(&other_workspace)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(!isolated.status.success());
    assert!(String::from_utf8_lossy(&isolated.stderr).contains("unknown configured or allocated"));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());

    let preview = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports", "http", "--dry-run"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(String::from_utf8_lossy(&preview.stdout).contains("Would terminate"));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
    assert_eq!(allocation_manifest_count(&lease_dir), 1);

    let cleanup = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "kill-ports"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(
        cleanup.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&cleanup.stdout),
        String::from_utf8_lossy(&cleanup.stderr)
    );
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", port)).is_err()
            && allocation_manifest_count(&lease_dir) == 0
    });
    assert!(String::from_utf8_lossy(&cleanup.stdout).contains("Cleared"));
}

#[test]
fn dev_supervises_targets_runs_prerequisites_and_restarts_on_dependency_changes() {
    let _serial = service_process_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // Keep both reservations open until launch so the OS cannot assign the
    // same ephemeral port to HTTP and the control server.
    let port_reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = port_reservation.local_addr().unwrap().port();
    let control_port_reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let control_port = control_port_reservation.local_addr().unwrap().port();
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
command = "sh -c 'echo SERVICE_STDOUT; echo SERVICE_STDERR >&2; if [ -n \"${ASTER_AMBIENT_SECRET:-}\" ]; then echo AMBIENT_SECRET_LEAKED >> ../events.log; fi; echo INHERITED:$ASTER_ALLOWED_VALUE >> ../events.log; echo START:$ASTER_SERVICE_PORT >> ../events.log; python3 -m http.server {port} & server=$!; echo $server > ../child.pid; wait $server'"
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
    let stdout_log = tempfile::NamedTempFile::new().unwrap();
    let stderr_log = tempfile::NamedTempFile::new().unwrap();

    // Process boundary: launch the public CLI, which starts a real prerequisite
    // process and a real HTTP child in its own process group.
    drop(port_reservation);
    drop(control_port_reservation);
    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "--no-ui"])
        .current_dir(root)
        .env("ASTER_AMBIENT_SECRET", "must-not-reach-service")
        .env("ASTER_ALLOWED_VALUE", "explicitly-allowed")
        .stdout(stdout_log.reopen().unwrap())
        .stderr(stderr_log.reopen().unwrap())
        .spawn()
        .unwrap();
    let events = root.join("events.log");
    let started = condition_met(Duration::from_secs(30), || {
        occurrences(&events, "PREPARE") >= 1
            && occurrences(&events, &format!("START:{port}")) >= 1
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    if !started {
        fail_with_process_diagnostics(
            &mut aster,
            &events,
            stdout_log.path(),
            stderr_log.path(),
            "service did not start",
        );
    }
    assert_eq!(occurrences(&events, "AMBIENT_SECRET_LEAKED"), 0);
    assert!(fs::read_to_string(&events)
        .unwrap()
        .contains("INHERITED:explicitly-allowed"));
    let worktree = root.file_name().unwrap();
    let durable_log = root.join(".aster/logs").join(worktree).join("web/logs.txt");
    wait_until(Duration::from_secs(5), || {
        let contents = fs::read_to_string(&durable_log).unwrap_or_default();
        contents.contains("SERVICE_STDOUT")
            && contents.contains("SERVICE_STDERR")
            && contents.contains("starting //app:dev")
    });
    assert!(fs::metadata(&durable_log).unwrap().len() <= 10 * 1024 * 1024);
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
    let suppressed_restart_completed = condition_met(Duration::from_secs(20), || {
        occurrences(&events, "PREPARE") >= 2
    });
    if !suppressed_restart_completed {
        fail_with_process_diagnostics(
            &mut aster,
            &events,
            stdout_log.path(),
            stderr_log.path(),
            "suppressed-path restart did not complete",
        );
    }
    thread::sleep(Duration::from_secs(2));

    // Watch boundary: mutate the transitive library dependency, not the service
    // directory. Mutate it again while the deliberately slow prerequisite is
    // running; that genuine source event must survive the restart cooldown.
    fs::write(root.join("lib/src/input.js"), "second").unwrap();
    let dependency_restart_started = condition_met(Duration::from_secs(20), || {
        occurrences(&events, "PREPARE") >= 3
    });
    if !dependency_restart_started {
        fail_with_process_diagnostics(
            &mut aster,
            &events,
            stdout_log.path(),
            stderr_log.path(),
            "dependency restart did not start",
        );
    }
    thread::sleep(Duration::from_millis(250));
    fs::write(root.join("lib/src/input.js"), "third").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline
        && (occurrences(&events, "PREPARE") < 4 || TcpStream::connect(("127.0.0.1", port)).is_err())
    {
        thread::sleep(Duration::from_millis(100));
    }
    if occurrences(&events, "PREPARE") < 4 || TcpStream::connect(("127.0.0.1", port)).is_err() {
        fail_with_process_diagnostics(
            &mut aster,
            &events,
            stdout_log.path(),
            stderr_log.path(),
            "restart did not settle",
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
    // A killed grandchild can remain as a zombie briefly on hosted macOS
    // runners. It is no longer executing, so treat that as terminated while
    // still rejecting any live descendant.
    wait_until(Duration::from_secs(5), || !process_is_running(child_pid));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn dev_restores_through_normal_shutdown_when_every_service_fails_to_start() {
    let _serial = service_process_test();
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
        .args(["services", "up", "--no-ui"])
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
    let _serial = service_process_test();
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
        .args(["services", "up", "--no-ui"])
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
    let _serial = service_process_test();
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
        .args(["services", "up", "--no-ui"])
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

#[test]
fn services_up_selects_an_optional_service_group() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();

    for project in ["platform", "intern-data", "intern-fe"] {
        let directory = root.join(project);
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("package.json"),
            format!(r#"{{"name":"{project}"}}"#),
        )
        .unwrap();
        fs::write(
            directory.join("aster.toml"),
            "[targets.dev]\ncommand = \"sh -c true\"\nstream = true\n",
        )
        .unwrap();
    }

    fs::write(
        root.join("aster.toml"),
        r#"
[dev]
control_port = "fallback-control"

[dev.ports]
fallback-control = 5100
main-control = 5101
intern-control = 5102

[dev.service_groups]
main = { services = ["platform"], control_port = "main-control" }
intern = { services = ["intern-data", "intern-fe"], control_port = "intern-control" }

[dev.services.platform]
target = "//platform:dev"

[dev.services.intern-data]
target = "//intern-data:dev"

[dev.services.intern-fe]
target = "//intern-fe:dev"
"#,
    )
    .unwrap();

    let ungrouped = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "--dry-run"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(ungrouped.status.success(), "{ungrouped:?}");
    let stderr = String::from_utf8_lossy(&ungrouped.stderr);
    assert!(stderr.contains("platform -> //platform:dev"), "{stderr}");
    assert!(!stderr.contains("intern-data ->"), "{stderr}");
    assert!(!stderr.contains("intern-fe ->"), "{stderr}");
    assert!(stderr.contains("control :5101"), "{stderr}");

    let intern = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "intern", "--dry-run"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(intern.status.success(), "{intern:?}");
    let stderr = String::from_utf8_lossy(&intern.stderr);
    assert!(!stderr.contains("platform ->"), "{stderr}");
    assert!(
        stderr.contains("intern-data -> //intern-data:dev"),
        "{stderr}"
    );
    assert!(stderr.contains("intern-fe -> //intern-fe:dev"), "{stderr}");
    assert!(stderr.contains("control :5102"), "{stderr}");

    let missing = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "missing", "--dry-run"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("unknown service group 'missing'"));
}

#[test]
fn concurrent_service_groups_bind_distinct_control_ports() {
    let _serial = service_process_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let alpha_reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let alpha_port = alpha_reservation.local_addr().unwrap().port();
    let beta_reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let beta_port = beta_reservation.local_addr().unwrap().port();

    fs::create_dir(root.join(".git")).unwrap();
    for project in ["alpha", "beta"] {
        let directory = root.join(project);
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("package.json"),
            format!(r#"{{"name":"{project}"}}"#),
        )
        .unwrap();
        fs::write(
            directory.join("aster.toml"),
            "[targets.dev]\ncommand = \"sh -c 'while true; do sleep 1; done'\"\nstream = true\n",
        )
        .unwrap();
    }
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev.ports]
alpha-control = {alpha_port}
beta-control = {beta_port}

[dev.service_groups]
alpha = {{ services = ["alpha"], control_port = "alpha-control" }}
beta = {{ services = ["beta"], control_port = "beta-control" }}

[dev.services.alpha]
target = "//alpha:dev"

[dev.services.beta]
target = "//beta:dev"
"#
        ),
    )
    .unwrap();
    drop(alpha_reservation);
    drop(beta_reservation);

    let mut alpha = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "alpha", "--no-ui", "--no-watch"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut beta = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "beta", "--no-ui", "--no-watch"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let (alpha_token_path, alpha_token) = wait_for_control_token(alpha_port, alpha.id());
    let (beta_token_path, beta_token) = wait_for_control_token(beta_port, beta.id());
    assert_eq!(
        control_request(alpha_port, r#"{"command":"status"}"#)["ok"],
        true
    );
    assert_eq!(
        control_request(beta_port, r#"{"command":"status"}"#)["ok"],
        true
    );

    for (port, token) in [(alpha_port, alpha_token), (beta_port, beta_token)] {
        let request = serde_json::json!({"command": "shutdown", "token": token}).to_string();
        assert_eq!(control_request(port, &request)["ok"], true);
    }
    assert!(alpha.wait().unwrap().success());
    assert!(beta.wait().unwrap().success());
    assert!(!alpha_token_path.exists());
    assert!(!beta_token_path.exists());
}

#[test]
fn services_logs_writes_raw_log_text_when_stdout_is_piped() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("aster.toml"),
        "[dev.services.api]\ntarget = \"//api:dev\"\n",
    )
    .unwrap();
    let log = root
        .join(".aster/logs")
        .join(root.file_name().unwrap())
        .join("api/logs.txt");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, "ready\nERROR exploded\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "logs", "api"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"ready\nERROR exploded\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn services_logs_rejects_unknown_services_and_missing_logs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("aster.toml"),
        "[dev.services.api]\ntarget = \"//api:dev\"\n",
    )
    .unwrap();

    let unknown = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "logs", "missing"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr)
        .contains("unknown service 'missing'; configured services: api"));

    let missing = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "logs", "api"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no logs found for service 'api'"));
}
