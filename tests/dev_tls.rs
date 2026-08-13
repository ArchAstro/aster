#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rcgen::generate_simple_self_signed;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(condition(), "condition not met within {timeout:?}");
}

#[test]
fn configured_tls_routes_replace_upstream_localhost_open_urls_in_public_plan() {
    // Setup: three ordinary services expose named internal ports; the selected
    // TLS edge publishes two of them and leaves one intentionally private.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    for project in ["frontend", "gateway", "worker"] {
        fs::create_dir(root.join(project)).unwrap();
        fs::write(
            root.join(project).join("package.json"),
            format!(r#"{{"name":"{project}"}}"#),
        )
        .unwrap();
        fs::write(
            root.join(project).join("aster.toml"),
            "[targets.dev]\ncommand = \"sh -c true\"\nstream = true\n",
        )
        .unwrap();
    }
    fs::write(
        root.join("aster.toml"),
        r#"
[dev.ports]
https = 8443
frontend = 3100
gateway = 3800
worker = 3900

[dev.service_groups]
intern = ["frontend", "gateway", "worker", "edge"]

[dev.services.frontend]
target = "//frontend:dev"
port = "frontend"
open_path = "/login"

[dev.services.gateway]
target = "//gateway:dev"
port = "gateway"

[dev.services.worker]
target = "//worker:dev"
port = "worker"

[dev.services.edge]
port = "https"
tls_proxy = { certificate_hosts = ["intern.dev", "*.local.sites.intern.dev"], open_host = "intern.dev", routes = [{ host = "intern.dev", upstream_port = "frontend" }, { host_suffix = ".sites.intern.dev", open_host = "test.local.sites.intern.dev", upstream_port = "gateway" }] }
"#,
    )
    .unwrap();

    // Public boundary: the same resolved plan feeds the dashboard [open]
    // action; dry-run renders those concrete URLs without launching services.
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "intern", "--dry-run"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let plan = String::from_utf8_lossy(&output.stderr);

    // Observable result: routed services open through trusted HTTPS, including
    // their own path, while an unpublished service retains localhost fallback.
    assert!(
        plan.contains("frontend :3100 -> //frontend:dev [open https://intern.dev:8443/login]"),
        "{plan}"
    );
    assert!(
        plan.contains(
            "gateway :3800 -> //gateway:dev [open https://test.local.sites.intern.dev:8443]"
        ),
        "{plan}"
    );
    assert!(
        plan.contains("worker :3900 -> //worker:dev [open http://localhost:3900]"),
        "{plan}"
    );
}

#[test]
fn tls_setup_generates_trusted_service_certificate_with_fake_mkcert() {
    // Setup: a fake mkcert executable records both trust and generation calls
    // while writing the exact files Aster requested.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("aster.toml"),
        r#"
[dev.ports]
https = 8443
upstream = 3000

[dev.services.edge]
port = "https"
tls_proxy = { certificate_hosts = ["intern.dev", "*.local.sites.intern.dev"], open_host = "intern.dev", routes = [{ host = "intern.dev", upstream_port = "upstream" }] }
"#,
    )
    .unwrap();
    let log = root.join("mkcert.log");
    let fake = root.join("mkcert");
    fs::write(
        &fake,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$ASTER_TEST_MKCERT_LOG"
test "$1" = "-install" && exit 0
while test "$#" -gt 0; do
  case "$1" in
    -cert-file) cert=$2; shift 2 ;;
    -key-file) key=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf 'certificate' > "$cert"
printf 'private-key' > "$key"
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

    // Boundary crossing: invoke the public CLI, which invokes mkcert twice.
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "tls", "setup", "edge"])
        .current_dir(root)
        .env("ASTER_MKCERT_BIN", &fake)
        .env("ASTER_TEST_MKCERT_LOG", &log)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    // Observable outcomes: trust is explicit, SANs are requested, and the key
    // is private in the workspace-scoped durable certificate directory.
    let calls = fs::read_to_string(log).unwrap();
    assert!(calls.lines().any(|line| line == "-install"), "{calls}");
    assert!(
        calls.contains("intern.dev *.local.sites.intern.dev"),
        "{calls}"
    );
    let cert_dir = root.join(".aster/tls/edge");
    assert_eq!(
        fs::metadata(cert_dir.join("key.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(cert_dir.join("cert.pem").is_file());
}

#[test]
fn service_group_serves_two_https_hosts_through_real_tls_and_http_boundaries() {
    // Setup: two real loopback HTTP services and a real certificate trusted by
    // this test client model the frontend and wildcard Sites gateway.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join(".git")).unwrap();
    let (front_listener, front_port) = reserve_port();
    let (site_listener, site_port) = reserve_port();
    let (tls_reservation, tls_port) = reserve_port();
    let (control_reservation, control_port) = reserve_port();
    let front = serve_http(front_listener, "frontend");
    let site = serve_http(site_listener, "site-gateway");

    fs::write(
        root.join("aster.toml"),
        format!(
            r#"
[dev]
control_port = "control"

[dev.ports]
https = {tls_port}
control = {control_port}
frontend = {front_port}
gateway = {site_port}

[dev.service_groups]
intern = ["intern-edge"]

[dev.services.intern-edge]
port = "https"
tls_proxy = {{ certificate_hosts = ["intern.dev", "*.local.sites.intern.dev"], open_host = "intern.dev", routes = [{{ host = "intern.dev", upstream_port = "frontend" }}, {{ host_suffix = ".sites.intern.dev", open_host = "test.local.sites.intern.dev", upstream_port = "gateway" }}] }}
"#
        ),
    )
    .unwrap();
    let certified = generate_simple_self_signed(vec![
        "intern.dev".to_string(),
        "*.local.sites.intern.dev".to_string(),
    ])
    .unwrap();
    let cert_dir = root.join(".aster/tls/intern-edge");
    fs::create_dir_all(&cert_dir).unwrap();
    fs::write(cert_dir.join("cert.pem"), certified.cert.pem()).unwrap();
    fs::write(
        cert_dir.join("key.pem"),
        certified.signing_key.serialize_pem(),
    )
    .unwrap();
    let cert_der = CertificateDer::from(certified.cert.der().to_vec());

    // Process boundary: launch the public Aster supervisor and its built-in TLS
    // service as a member of the named Intern group.
    drop(tls_reservation);
    drop(control_reservation);
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "intern", "--no-ui", "--no-watch"])
        .current_dir(root)
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(8), || {
        TcpStream::connect(("127.0.0.1", tls_port)).is_ok()
    });

    // Observable behavior: exact and suffix hosts cross real TLS and HTTP
    // sockets, preserve logical Host, and select different upstream services.
    let front_authority = format!("intern.dev:{tls_port}");
    let front_response = https_get(tls_port, "intern.dev", &front_authority, cert_der.clone());
    assert!(front_response.contains("200 OK"), "{front_response}");
    assert!(
        front_response.contains(&format!(
            "frontend host={front_authority} forwarded={front_authority} proto=https"
        )),
        "{front_response}"
    );
    let site_response = https_get(
        tls_port,
        "test.local.sites.intern.dev",
        "test.local.sites.intern.dev",
        cert_der.clone(),
    );
    assert!(
        site_response.contains(
            "site-gateway host=test.local.sites.intern.dev forwarded=test.local.sites.intern.dev proto=https"
        ),
        "{site_response}"
    );
    let unknown = https_get(tls_port, "intern.dev", "unknown.test", cert_der);
    assert!(unknown.contains("421 Misdirected Request"), "{unknown}");
    let malformed = https_get(
        tls_port,
        "intern.dev",
        "intern.dev:443@evil.example",
        CertificateDer::from(certified.cert.der().to_vec()),
    );
    assert!(malformed.contains("400 Bad Request"), "{malformed}");
    assert!(
        malformed.contains("missing or invalid Host header"),
        "{malformed}"
    );
    assert_tls_upgrade_echo(tls_port, "intern.dev", certified.cert.der().to_vec());

    // Lifecycle outcome: authenticated control shutdown stops the group and
    // leaves durable evidence that the TLS edge reached readiness.
    let (token_path, token) = wait_for_control_token(control_port, aster.id());
    let response = control_request(
        control_port,
        &serde_json::json!({"command":"shutdown", "token":token}).to_string(),
    );
    assert_eq!(response["ok"], true);
    assert!(aster.wait().unwrap().success());
    assert!(!token_path.exists());
    assert!(TcpStream::connect(("127.0.0.1", tls_port)).is_err());
    let durable = root
        .join(".aster/logs")
        .join(root.file_name().unwrap())
        .join("intern-edge/logs.txt");
    assert!(fs::read_to_string(durable)
        .unwrap()
        .contains("TLS edge 'intern-edge' ready"));
    drop(front);
    drop(site);
}

fn serve_http(listener: TcpListener, label: &'static str) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut host = String::new();
            let mut forwarded_host = String::new();
            let mut proto = String::new();
            let mut upgrade = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("get /upgrade ") {
                    upgrade = true;
                }
                if lower.starts_with("host:") {
                    host = line[5..].trim().to_string();
                }
                if lower.starts_with("x-forwarded-proto:") {
                    proto = line[18..].trim().to_string();
                }
                if lower.starts_with("x-forwarded-host:") {
                    forwarded_host = line[17..].trim().to_string();
                }
            }
            if upgrade {
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: test\r\n\r\n"
                )
                .unwrap();
                stream.flush().unwrap();
                let mut payload = [0_u8; 4];
                stream.read_exact(&mut payload).unwrap();
                stream.write_all(&payload).unwrap();
                continue;
            }
            let body = format!("{label} host={host} forwarded={forwarded_host} proto={proto}");
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    })
}

fn assert_tls_upgrade_echo(port: u16, host: &str, cert: Vec<u8>) {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(cert)).unwrap();
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connection = ClientConnection::new(
        Arc::new(config),
        ServerName::try_from(host.to_string()).unwrap(),
    )
    .unwrap();
    let socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut tls = StreamOwned::new(connection, socket);

    write!(
        tls,
        "GET /upgrade HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: test\r\n\r\n"
    )
    .unwrap();
    tls.flush().unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        tls.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers).unwrap();
    assert!(headers.contains("101 Switching Protocols"), "{headers}");

    tls.write_all(b"ping").unwrap();
    tls.flush().unwrap();
    let mut echoed = [0_u8; 4];
    tls.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"ping");
}

fn https_get(port: u16, sni: &str, host: &str, cert: CertificateDer<'static>) -> String {
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connection = ClientConnection::new(
        Arc::new(config),
        ServerName::try_from(sni.to_string()).unwrap(),
    )
    .unwrap();
    let socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut tls = StreamOwned::new(connection, socket);
    write!(
        tls,
        "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    tls.read_to_string(&mut response).unwrap();
    response
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

fn control_request(port: u16, request: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}
