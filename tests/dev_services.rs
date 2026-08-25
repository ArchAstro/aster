#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
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

fn http_get(port: u16) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
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
fn optional_service_proxy_preserves_the_advertised_port_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let lease_dir = root.join("leases");
    let advertised_reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let advertised_port = advertised_reservation.local_addr().unwrap().port();
    let upstream_reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let upstream_port = upstream_reservation.local_addr().unwrap().port();

    // Setup: one real HTTP service and one real TCP proxy are ordinary stream targets.
    fs::create_dir(root.join(".git")).unwrap();
    for project in ["app", "proxy"] {
        fs::create_dir(root.join(project)).unwrap();
        fs::write(
            root.join(project).join("package.json"),
            format!(r#"{{"name":"{project}"}}"#),
        )
        .unwrap();
    }
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev.ports]
platform = {advertised_port}
platform-upstream = {upstream_port}

[dev.services.platform]
target = "//app:dev"
port = "platform"
port_env = {{ PORT = "platform" }}
proxy = {{ target = "//proxy:dev", upstream_port = "platform-upstream", env = {{ LISTEN_PORT = "{{proxy.listen_port}}", UPSTREAM_PORT = "{{proxy.upstream_port}}" }} }}
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        "[targets.dev]\ncommand = \"python3 server.py {port}\"\nstream = true\n",
    )
    .unwrap();
    fs::write(
        root.join("app/server.py"),
        r#"import http.server
import os
import sys

port = int(sys.argv[1])

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = f"app-port={port};env-port={os.environ['PORT']}".encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *_):
        pass

server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
with open("../app-events.log", "a") as events:
    events.write(f"{port}:{os.environ['PORT']}\n")
server.serve_forever()
"#,
    )
    .unwrap();
    fs::write(
        root.join("proxy/aster.toml"),
        "[targets.dev]\ncommand = \"python3 proxy.py\"\nstream = true\n",
    )
    .unwrap();
    fs::write(
        root.join("proxy/proxy.py"),
        r#"import os
import select
import socket
import socketserver

listen_port = int(os.environ["LISTEN_PORT"])
upstream_port = int(os.environ["UPSTREAM_PORT"])
with open("../proxy-events.log", "a") as events:
    events.write(f"{listen_port}:{upstream_port}:{os.environ['ASTER_PROXY_LISTEN_PORT']}:{os.environ['ASTER_PROXY_UPSTREAM_PORT']}:{os.environ['ASTER_PROXY_SERVICE_NAME']}\n")
print(f"proxy-ready {listen_port}->{upstream_port}", flush=True)

class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        with socket.create_connection(("127.0.0.1", upstream_port)) as upstream:
            peers = {self.request: upstream, upstream: self.request}
            while True:
                readable, _, _ = select.select(list(peers), [], [])
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        return
                    peers[source].sendall(data)

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True

Server(("127.0.0.1", listen_port), Handler).serve_forever()
"#,
    )
    .unwrap();
    drop((advertised_reservation, upstream_reservation));

    let launch = |proxy: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aster"));
        command.args(["services", "up", "--no-ui", "--no-watch"]);
        if proxy {
            command.arg("--proxy");
        }
        let mode = if proxy { "proxied" } else { "direct" };
        command
            .current_dir(root)
            .env("ASTER_PORT_LEASE_DIR", &lease_dir)
            .stdout(Stdio::from(
                fs::File::create(root.join(format!("aster-{mode}.stdout"))).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(root.join(format!("aster-{mode}.stderr"))).unwrap(),
            ))
            .spawn()
            .unwrap()
    };

    // Proxy boundary: the advertised port belongs to the sidecar while the app receives the upstream port.
    let mut proxied = launch(true);
    let upstream_ready = condition_met(Duration::from_secs(20), || {
        TcpStream::connect(("127.0.0.1", upstream_port)).is_ok()
    });
    if !upstream_ready {
        fail_with_process_diagnostics(
            &mut proxied,
            &root.join("app-events.log"),
            &root.join("aster-proxied.stdout"),
            &root.join("aster-proxied.stderr"),
            "upstream app never bound its port",
        );
    }
    let expected_proxied_body = format!("app-port={upstream_port};env-port={upstream_port}");
    let mut last_proxy_attempt = String::new();
    let proxied_ready = condition_met(Duration::from_secs(20), || {
        match http_get(advertised_port) {
            Ok(response) => {
                last_proxy_attempt = format!("response: {response:?}");
                response.contains(&expected_proxied_body)
            }
            Err(error) => {
                last_proxy_attempt = format!("error: {error:?}");
                false
            }
        }
    });
    if !proxied_ready {
        fail_with_process_diagnostics(
            &mut proxied,
            &root.join("proxy-events.log"),
            &root.join("aster-proxied.stdout"),
            &root.join("aster-proxied.stderr"),
            &format!(
                "proxied request never reached the app; last client attempt: {last_proxy_attempt}; app events:\n{}",
                fs::read_to_string(root.join("app-events.log")).unwrap_or_default()
            ),
        );
    }
    assert_eq!(
        fs::read_to_string(root.join("app-events.log")).unwrap(),
        format!("{upstream_port}:{upstream_port}\n")
    );
    assert_eq!(
        fs::read_to_string(root.join("proxy-events.log")).unwrap(),
        format!("{advertised_port}:{upstream_port}:{advertised_port}:{upstream_port}:platform\n")
    );
    wait_until(Duration::from_secs(5), || {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(["services", "logs", "platform-proxy"])
            .current_dir(root)
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("proxy-ready")
            })
    });

    // Published state: service discovery still maps platform to its original advertised port.
    let ports = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["--json", "services", "ports"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(ports.status.success(), "{ports:?}");
    let report: serde_json::Value = serde_json::from_slice(&ports.stdout).unwrap();
    let instance = &report["instances"][0];
    assert_eq!(instance["services"][0]["name"], "platform");
    assert_eq!(instance["services"][0]["port_name"], "platform");
    assert_eq!(instance["services"][0]["port"], advertised_port);
    assert_eq!(instance["ports"]["platform"], advertised_port);
    assert_eq!(instance["ports"]["platform-upstream"], upstream_port);

    terminate_aster(&mut proxied);
    wait_until(Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", advertised_port)).is_err()
            && TcpStream::connect(("127.0.0.1", upstream_port)).is_err()
    });

    // Optional path: without the flag, the app returns to the same advertised port and no proxy starts.
    let mut direct = launch(false);
    let expected_direct_body = format!("app-port={advertised_port};env-port={advertised_port}");
    let direct_ready = condition_met(Duration::from_secs(20), || {
        http_get(advertised_port).is_ok_and(|response| response.contains(&expected_direct_body))
    });
    if !direct_ready {
        fail_with_process_diagnostics(
            &mut direct,
            &root.join("app-events.log"),
            &root.join("aster-direct.stdout"),
            &root.join("aster-direct.stderr"),
            "direct request never reached the app",
        );
    }
    assert_eq!(
        occurrences(&root.join("proxy-events.log"), ":platform\n"),
        1
    );
    terminate_aster(&mut direct);
}

#[test]
fn ports_reports_static_and_portless_services_from_the_running_instance() {
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
fn daemon_runtime_is_single_instance_and_exits_after_last_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let runtime_dir = root.join("daemon-runtime");
    let lease_dir = root.join("leases");
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("aster.toml"),
        "[dev.services.worker]\ntarget = \"//app:worker\"\n",
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        "[targets.worker]\ncommand = \"sleep 30\"\nstream = true\n",
    )
    .unwrap();

    let executable = Path::new(env!("CARGO_BIN_EXE_aster")).to_path_buf();
    let launch = || {
        let root = root.to_path_buf();
        let executable = executable.clone();
        thread::spawn(move || {
            aster::dev::launch_bundle(aster::dev::LaunchOptions {
                workspace: root,
                group: None,
                watch: false,
                use_cache: true,
                proxy: false,
                executable,
                environment: std::env::vars().collect(),
            })
            .unwrap()
        })
    };

    std::env::set_var("ASTER_DAEMON_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("ASTER_PORT_LEASE_DIR", &lease_dir);
    let first = launch();
    let second = launch();
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert_eq!(first.bundle.supervisor_pid, second.bundle.supervisor_pid);
    assert_eq!(first.bundle.services, vec!["worker"]);
    assert_ne!(first.status, second.status);
    let daemon_pid = aster::dev::ping_daemon().unwrap();
    assert!(process_is_running(daemon_pid as i32));
    assert_eq!(aster::dev::list_workspace_bundles(root).unwrap().len(), 1);
    let attach_socket = aster::dev::attach_bundle(root, None).unwrap();
    assert_eq!(
        first.bundle.attach_socket.as_deref(),
        Some(attach_socket.as_path())
    );
    assert!(attach_socket.exists());

    let stopped = aster::dev::stop_workspace_bundles(root, None).unwrap();
    assert_eq!(stopped.len(), 1);
    wait_until(Duration::from_secs(10), || {
        !runtime_dir.join("daemon.sock").exists()
            && !runtime_dir.join("daemon.pid").exists()
            && !process_is_running(daemon_pid as i32)
            && !process_is_running(first.bundle.supervisor_pid as i32)
    });
    std::env::remove_var("ASTER_DAEMON_RUNTIME_DIR");
    std::env::remove_var("ASTER_PORT_LEASE_DIR");
}

struct DaemonCliGuard {
    runtime_dir: std::path::PathBuf,
    lease_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

impl DaemonCliGuard {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aster"));
        command
            .current_dir(&self.cwd)
            .env("ASTER_DAEMON_RUNTIME_DIR", &self.runtime_dir)
            .env("ASTER_PORT_LEASE_DIR", &self.lease_dir);
        command
    }
}

impl Drop for DaemonCliGuard {
    fn drop(&mut self) {
        let _ = self
            .command()
            .args(["services", "daemon", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn write_daemon_workspace(root: &Path, port: u16, group: Option<&str>) {
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    let group_config = group.map_or_else(String::new, |group| {
        format!("[dev.service_groups]\n{group} = {{ services = [\"web\", \"worker\"] }}\n")
    });
    fs::write(
        root.join("aster.toml"),
        format!(
            "[dev]\ndaemon = true\n\n[dev.ports.http]\ndefault = {port}\n\n[dev.services.web]\ntarget = \"//app:web\"\nport = \"http\"\n\n[dev.services.worker]\ntarget = \"//app:worker\"\n\n{group_config}"
        ),
    )
    .unwrap();
    fs::write(
        root.join("app/aster.toml"),
        "[targets.web]\ncommand = \"python3 -m http.server {port}\"\nstream = true\n\n[targets.worker]\ncommand = \"sleep 60\"\nstream = true\n",
    )
    .unwrap();
}

fn write_proxy_daemon_workspace(root: &Path, advertised_port: u16, upstream_port: u16) {
    fs::create_dir_all(root.join(".git")).unwrap();
    for project in ["app", "proxy"] {
        fs::create_dir_all(root.join(project)).unwrap();
        fs::write(
            root.join(project).join("package.json"),
            format!(r#"{{"name":"{project}"}}"#),
        )
        .unwrap();
        fs::write(
            root.join(project).join("aster.toml"),
            "[targets.dev]\ncommand = \"sleep 60\"\nstream = true\n",
        )
        .unwrap();
    }
    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev.ports]
http = {advertised_port}
http-upstream = {upstream_port}

[dev.services.web]
target = "//app:dev"
port = "http"
proxy = {{ target = "//proxy:dev", upstream_port = "http-upstream" }}
"#
        ),
    )
    .unwrap();
}

#[test]
fn daemon_rejects_reattach_with_a_different_proxy_mode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let advertised = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let advertised_port = advertised.local_addr().unwrap().port();
    let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    write_proxy_daemon_workspace(&root, advertised_port, upstream_port);
    drop((advertised, upstream));
    let guard = DaemonCliGuard {
        runtime_dir: temp.path().join("runtime"),
        lease_dir: temp.path().join("leases"),
        cwd: root,
    };

    let first = guard
        .command()
        .args([
            "--json",
            "services",
            "up",
            "--daemon",
            "--no-watch",
            "--proxy",
        ])
        .output()
        .unwrap();
    assert!(first.status.success(), "{first:?}");
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_json["status"], "started");
    assert_eq!(
        first_json["bundle"]["services"],
        serde_json::json!(["web", "web-proxy"])
    );
    assert_eq!(first_json["bundle"]["ports"]["http"], advertised_port);
    assert_eq!(
        first_json["bundle"]["ports"]["http-upstream"],
        upstream_port
    );

    let mismatch = guard
        .command()
        .args(["--json", "services", "up", "--daemon", "--no-watch"])
        .output()
        .unwrap();
    assert!(!mismatch.status.success(), "{mismatch:?}");
    assert!(
        String::from_utf8_lossy(&mismatch.stderr)
            .contains("already running with proxies; stop it before launching without proxies"),
        "{mismatch:?}"
    );

    let same_mode = guard
        .command()
        .args([
            "--json",
            "services",
            "up",
            "--daemon",
            "--no-watch",
            "--proxy",
        ])
        .output()
        .unwrap();
    assert!(same_mode.status.success(), "{same_mode:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&same_mode.stdout).unwrap()["status"],
        "already_running"
    );
}

#[test]
fn daemon_cli_manages_and_lists_worktree_bundles() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    write_daemon_workspace(&root, port, Some("intern"));
    let guard = DaemonCliGuard {
        runtime_dir: temp.path().join("runtime"),
        lease_dir: temp.path().join("leases"),
        cwd: root.clone(),
    };

    let absent = guard
        .command()
        .args(["--json", "services", "list"])
        .output()
        .unwrap();
    assert!(absent.status.success(), "{absent:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&absent.stdout).unwrap()["bundles"],
        serde_json::json!([])
    );
    assert!(!guard.runtime_dir.join("daemon.sock").exists());

    let first = guard
        .command()
        .args([
            "--json",
            "services",
            "up",
            "intern",
            "--no-ui",
            "--no-watch",
        ])
        .output()
        .unwrap();
    assert!(first.status.success(), "{first:?}");
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["status"], "started");
    assert_eq!(
        first["bundle"]["services"],
        serde_json::json!(["web", "worker"])
    );
    assert_eq!(first["bundle"]["ports"]["http"], port);

    let second = guard
        .command()
        .args([
            "--json",
            "services",
            "up",
            "intern",
            "--daemon",
            "--no-watch",
        ])
        .output()
        .unwrap();
    assert!(second.status.success(), "{second:?}");
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["status"], "already_running");
    assert_eq!(
        second["bundle"]["supervisor_pid"],
        first["bundle"]["supervisor_pid"]
    );

    let listed = guard
        .command()
        .args(["--json", "services", "list"])
        .output()
        .unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        listed["workspace"],
        root.canonicalize().unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(listed["bundles"][0]["group"], "intern");
    assert_eq!(listed["bundles"][0]["display_group"], "intern");
    assert_eq!(listed["bundles"][0]["state"], "running");
    assert_eq!(
        listed["bundles"][0]["services"],
        serde_json::json!(["web", "worker"])
    );
    assert_eq!(listed["bundles"][0]["ports"]["http"], port);

    let down = guard
        .command()
        .args(["--json", "services", "down", "intern"])
        .output()
        .unwrap();
    assert!(down.status.success(), "{down:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&down.stdout).unwrap()["stopped"],
        1
    );
    wait_until(Duration::from_secs(10), || {
        !guard.runtime_dir.join("daemon.sock").exists()
    });
}

#[test]
fn daemon_cli_isolates_worktrees_and_supports_global_stop() {
    let temp = tempfile::tempdir().unwrap();
    let first_root = temp.path().join("first");
    let second_root = temp.path().join("second");
    let first_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let first_port = first_listener.local_addr().unwrap().port();
    let second_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let second_port = second_listener.local_addr().unwrap().port();
    drop((first_listener, second_listener));
    write_daemon_workspace(&first_root, first_port, None);
    write_daemon_workspace(&second_root, second_port, None);
    let mut guard = DaemonCliGuard {
        runtime_dir: temp.path().join("runtime"),
        lease_dir: temp.path().join("leases"),
        cwd: first_root.clone(),
    };

    for root in [&first_root, &second_root] {
        guard.cwd.clone_from(root);
        let up = guard
            .command()
            .args(["services", "up", "--daemon", "--no-watch"])
            .output()
            .unwrap();
        assert!(up.status.success(), "{up:?}");
    }
    for (root, other) in [(&first_root, &second_root), (&second_root, &first_root)] {
        guard.cwd.clone_from(root);
        let list = guard
            .command()
            .args(["--json", "services", "list"])
            .output()
            .unwrap();
        let list = String::from_utf8(list.stdout).unwrap();
        assert!(list.contains(root.canonicalize().unwrap().to_string_lossy().as_ref()));
        assert!(!list.contains(other.canonicalize().unwrap().to_string_lossy().as_ref()));
    }

    guard.cwd.clone_from(&first_root);
    assert!(guard
        .command()
        .args(["services", "down"])
        .status()
        .unwrap()
        .success());
    guard.cwd = temp.path().to_path_buf();
    assert!(guard
        .command()
        .args(["services", "daemon", "stop"])
        .status()
        .unwrap()
        .success());
    wait_until(Duration::from_secs(10), || {
        !guard.runtime_dir.join("daemon.sock").exists()
            && TcpStream::connect(("127.0.0.1", first_port)).is_err()
            && TcpStream::connect(("127.0.0.1", second_port)).is_err()
    });
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
