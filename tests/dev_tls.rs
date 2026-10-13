#![cfg(unix)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;

use rcgen::{
    generate_simple_self_signed, BasicConstraints, CertificateParams, DnType, IsCa, Issuer,
    KeyPair, KeyUsagePurpose,
};
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

struct OwnedTlsFixturePorts {
    https: u16,
    frontend: u16,
    gateway: u16,
    worker: u16,
    listeners: Vec<TcpListener>,
}

fn reserve_distinct_ports(count: usize) -> Vec<(TcpListener, u16)> {
    let mut reserved = Vec::with_capacity(count);
    while reserved.len() < count {
        let (listener, port) = reserve_port();
        if reserved.iter().any(|(_, held)| *held == port) {
            continue;
        }
        reserved.push((listener, port));
    }
    reserved
}

fn owned_tls_fixture_ports() -> OwnedTlsFixturePorts {
    let mut reserved = reserve_distinct_ports(4);
    let (https_listener, https) = reserved.pop().unwrap();
    let (frontend_listener, frontend) = reserved.pop().unwrap();
    let (gateway_listener, gateway) = reserved.pop().unwrap();
    let (worker_listener, worker) = reserved.pop().unwrap();
    OwnedTlsFixturePorts {
        https,
        frontend,
        gateway,
        worker,
        listeners: vec![
            https_listener,
            frontend_listener,
            gateway_listener,
            worker_listener,
        ],
    }
}

fn write_tls_open_url_workspace(root: &std::path::Path, ports: &OwnedTlsFixturePorts) {
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
        format!(
            r#"
[dev.ports]
https = {https}
frontend = {frontend}
gateway = {gateway}
worker = {worker}

[dev.service_groups]
web = ["frontend", "gateway", "worker", "edge"]

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
tls_proxy = {{ certificate_hosts = ["app.example.test"], open_host = "app.example.test", routes = [{{ host = "app.example.test", upstream_port = "frontend" }}, {{ host_suffix = ".example.test", open_host = "demo.team.example.test", upstream_port = "gateway" }}] }}
"#,
            https = ports.https,
            frontend = ports.frontend,
            gateway = ports.gateway,
            worker = ports.worker,
        ),
    )
    .unwrap();
}

fn allocation_manifests(lease_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(lease_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("allocation-") && name.ends_with(".json"))
        })
        .collect()
}

fn port_lock_is_held_by_another_process(path: &std::path::Path) -> bool {
    let probe = Command::new("python3")
        .args([
            "-c",
            "import fcntl,sys\ntry:\n    f=open(sys.argv[1],'a+')\n    fcntl.flock(f.fileno(), fcntl.LOCK_EX|fcntl.LOCK_NB)\nexcept BlockingIOError:\n    sys.exit(1)",
            path.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    !probe.success()
}

fn hold_port_lease(lease_dir: &std::path::Path, port: u16) -> File {
    fs::create_dir_all(lease_dir).unwrap();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lease_dir.join(format!("{port}.lock")))
        .unwrap();
    file.try_lock_exclusive().unwrap();
    file
}

#[test]
fn configured_tls_routes_replace_upstream_localhost_open_urls_in_public_plan() {
    // Setup: three ordinary services expose test-owned internal ports; the
    // selected TLS edge publishes two of them and leaves one intentionally
    // private. Listeners stay bound only until dry-run needs the ports.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let lease_dir = temp.path().join("port-leases");
    let mut ports = owned_tls_fixture_ports();
    write_tls_open_url_workspace(root, &ports);

    // Public boundary: the same resolved plan feeds the dashboard [open]
    // action; dry-run renders those concrete URLs without launching services.
    ports.listeners.clear();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "web", "--dry-run"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let plan = String::from_utf8_lossy(&output.stderr);

    // Observable result: routed services open through trusted HTTPS, including
    // their own path, while an unpublished service retains localhost fallback.
    assert!(
        plan.contains(&format!(
            "frontend :{} -> //frontend:dev [open https://app.example.test:{}/login]",
            ports.frontend, ports.https
        )),
        "{plan}"
    );
    assert!(
        plan.contains(&format!(
            "gateway :{} -> //gateway:dev [open https://demo.team.example.test:{}]",
            ports.gateway, ports.https
        )),
        "{plan}"
    );
    assert!(
        plan.contains(&format!(
            "worker :{} -> //worker:dev [open http://localhost:{}]",
            ports.worker, ports.worker
        )),
        "{plan}"
    );
    assert!(
        allocation_manifests(&lease_dir).is_empty(),
        "successful dry-run left a port allocation manifest"
    );

    // Occupied-port failure: the fixture keeps its own listener. Dry-run must
    // name that port and must not kill the unrelated holder.
    let occupied = TcpListener::bind(("127.0.0.1", ports.frontend)).unwrap();
    let occupied_failure = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "web", "--dry-run"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(!occupied_failure.status.success(), "{occupied_failure:?}");
    let occupied_error = String::from_utf8_lossy(&occupied_failure.stderr);
    assert!(
        occupied_error.contains(&format!("frontend={}", ports.frontend)),
        "{occupied_error}"
    );
    assert!(
        occupied_error.contains("already allocated or listening"),
        "{occupied_error}"
    );
    assert!(
        occupied.local_addr().unwrap().port() == ports.frontend,
        "occupied fixture listener was closed"
    );
    assert!(
        allocation_manifests(&lease_dir).is_empty(),
        "occupied-port failure left a port allocation manifest"
    );
    drop(occupied);

    // Interruption: stop the fixture-owned dry-run before wait. The signal
    // targets only that child. Its lease directory is test-owned and must not
    // keep an allocation manifest after the child is gone.
    let mut interrupted = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "web", "--dry-run"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    unsafe {
        libc::kill(interrupted.id() as i32, libc::SIGTERM);
    }
    let _ = interrupted.wait();
    assert!(
        allocation_manifests(&lease_dir).is_empty(),
        "interrupted dry-run left a port allocation manifest"
    );

    // WouldBlock: another owner holds the same port lock file PortAllocator
    // uses. Dry-run fails closed and leaves that lock held.
    let held_lock = hold_port_lease(&lease_dir, ports.https);
    let blocked = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "web", "--dry-run"])
        .current_dir(root)
        .env("ASTER_PORT_LEASE_DIR", &lease_dir)
        .output()
        .unwrap();
    assert!(!blocked.status.success(), "{blocked:?}");
    let blocked_error = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        blocked_error.contains(&format!("https={}", ports.https)),
        "{blocked_error}"
    );
    assert!(
        blocked_error.contains("already allocated or listening"),
        "{blocked_error}"
    );
    assert!(
        port_lock_is_held_by_another_process(&lease_dir.join(format!("{}.lock", ports.https))),
        "WouldBlock fixture released the port lock"
    );
    drop(held_lock);
    assert!(
        allocation_manifests(&lease_dir).is_empty(),
        "WouldBlock failure left a port allocation manifest"
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
tls_proxy = { certificate_hosts = ["app.example.test", "*.local.example.test"], open_host = "app.example.test", routes = [{ host = "app.example.test", upstream_port = "upstream" }] }
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
        calls.contains("app.example.test *.local.example.test"),
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
fn suffix_route_issues_and_caches_exact_sni_certificate_through_real_tls() {
    // Setup: two real loopback HTTP services, a static frontend certificate,
    // and a fake mkcert issuer for a nested site hostname.
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
web = ["edge"]

[dev.services.edge]
port = "https"
inherit_env = ["ASTER_MKCERT_BIN", "ASTER_TEST_MKCERT_LOG", "ASTER_TEST_DYNAMIC_CERT", "ASTER_TEST_DYNAMIC_KEY"]
tls_proxy = {{ certificate_hosts = ["app.example.test"], open_host = "app.example.test", routes = [{{ host = "app.example.test", upstream_port = "frontend" }}, {{ host_suffix = ".example.test", open_host = "docs.acme.example.test", upstream_port = "gateway" }}] }}
"#
        ),
    )
    .unwrap();
    let certified = generate_simple_self_signed(vec!["app.example.test".to_string()]).unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Aster test local CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let site_key_pair = KeyPair::generate().unwrap();
    let mut site_params =
        CertificateParams::new(vec!["docs.acme.example.test".to_string()]).unwrap();
    site_params
        .distinguished_name
        .push(DnType::CommonName, "docs.acme.example.test");
    let site_certified = site_params.signed_by(&site_key_pair, &issuer).unwrap();
    let cert_dir = root.join(".aster/tls/edge");
    fs::create_dir_all(&cert_dir).unwrap();
    fs::write(cert_dir.join("cert.pem"), certified.cert.pem()).unwrap();
    fs::write(
        cert_dir.join("key.pem"),
        certified.signing_key.serialize_pem(),
    )
    .unwrap();
    let cert_der = CertificateDer::from(certified.cert.der().to_vec());
    let site_cert = root.join("site-cert.pem");
    let site_key = root.join("site-key.pem");
    fs::write(&site_cert, site_certified.pem()).unwrap();
    fs::write(&site_key, site_key_pair.serialize_pem()).unwrap();
    let mkcert_log = root.join("mkcert.log");
    let fake_mkcert = root.join("mkcert");
    fs::write(
        &fake_mkcert,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$ASTER_TEST_MKCERT_LOG"
while test "$#" -gt 0; do
  case "$1" in
    -cert-file) cert=$2; shift 2 ;;
    -key-file) key=$2; shift 2 ;;
    *) host=$1; shift ;;
  esac
done
test "$host" = "docs.acme.example.test" || exit 2
cp "$ASTER_TEST_DYNAMIC_CERT" "$cert"
cp "$ASTER_TEST_DYNAMIC_KEY" "$key"
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_mkcert, fs::Permissions::from_mode(0o700)).unwrap();

    // Process boundary: launch the public Aster supervisor and its built-in TLS
    // service as a member of a named service group.
    drop(tls_reservation);
    drop(control_reservation);
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut aster = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "web", "--no-ui", "--no-watch"])
        .current_dir(root)
        .env("ASTER_MKCERT_BIN", &fake_mkcert)
        .env("ASTER_TEST_MKCERT_LOG", &mkcert_log)
        .env("ASTER_TEST_DYNAMIC_CERT", &site_cert)
        .env("ASTER_TEST_DYNAMIC_KEY", &site_key)
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(8), || {
        TcpStream::connect(("127.0.0.1", tls_port)).is_ok()
    });

    // Observable behavior: exact and suffix hosts cross real TLS and HTTP
    // sockets, preserve logical Host, and select different upstream services.
    let front_authority = format!("app.example.test:{tls_port}");
    let front_response = https_get(
        tls_port,
        "app.example.test",
        &front_authority,
        cert_der.clone(),
    );
    assert!(front_response.contains("200 OK"), "{front_response}");
    assert!(
        front_response.contains(&format!(
            "frontend host={front_authority} forwarded={front_authority} proto=https"
        )),
        "{front_response}"
    );
    let site_response = https_get_with_roots(
        tls_port,
        "docs.acme.example.test",
        "docs.acme.example.test",
        vec![CertificateDer::from(ca_cert.der().to_vec())],
    );
    assert!(
        site_response.contains(
            "site-gateway host=docs.acme.example.test forwarded=docs.acme.example.test proto=https"
        ),
        "{site_response}"
    );
    let cached_site_response = https_get_with_roots(
        tls_port,
        "docs.acme.example.test",
        "docs.acme.example.test",
        vec![CertificateDer::from(ca_cert.der().to_vec())],
    );
    assert!(
        cached_site_response.contains("200 OK"),
        "{cached_site_response}"
    );
    assert_eq!(fs::read_to_string(&mkcert_log).unwrap().lines().count(), 1);
    assert_tls_handshake_rejected(tls_port, "outside.invalid", certified.cert.der().to_vec());
    assert_eq!(fs::read_to_string(&mkcert_log).unwrap().lines().count(), 1);
    let unknown = https_get(tls_port, "app.example.test", "unknown.test", cert_der);
    assert!(unknown.contains("421 Misdirected Request"), "{unknown}");
    let malformed = https_get(
        tls_port,
        "app.example.test",
        "app.example.test:443@evil.example",
        CertificateDer::from(certified.cert.der().to_vec()),
    );
    assert!(malformed.contains("400 Bad Request"), "{malformed}");
    assert!(
        malformed.contains("missing or invalid Host header"),
        "{malformed}"
    );
    assert_tls_upgrade_echo(tls_port, "app.example.test", certified.cert.der().to_vec());

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

    // Restart boundary: the next supervisor reuses the validated disk cache;
    // it does not invoke mkcert again for the same nested SNI hostname.
    let second_stdout = tempfile::NamedTempFile::new().unwrap();
    let second_stderr = tempfile::NamedTempFile::new().unwrap();
    let mut restarted = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["services", "up", "web", "--no-ui", "--no-watch"])
        .current_dir(root)
        .env("ASTER_MKCERT_BIN", &fake_mkcert)
        .env("ASTER_TEST_MKCERT_LOG", &mkcert_log)
        .env("ASTER_TEST_DYNAMIC_CERT", &site_cert)
        .env("ASTER_TEST_DYNAMIC_KEY", &site_key)
        .stdout(second_stdout.reopen().unwrap())
        .stderr(second_stderr.reopen().unwrap())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(8), || {
        TcpStream::connect(("127.0.0.1", tls_port)).is_ok()
    });
    let persisted_site_response = https_get_with_roots(
        tls_port,
        "docs.acme.example.test",
        "docs.acme.example.test",
        vec![CertificateDer::from(ca_cert.der().to_vec())],
    );
    assert!(
        persisted_site_response.contains("200 OK"),
        "{persisted_site_response}"
    );
    assert_eq!(fs::read_to_string(&mkcert_log).unwrap().lines().count(), 1);
    let (second_token_path, second_token) = wait_for_control_token(control_port, restarted.id());
    let response = control_request(
        control_port,
        &serde_json::json!({"command":"shutdown", "token":second_token}).to_string(),
    );
    assert_eq!(response["ok"], true);
    assert!(restarted.wait().unwrap().success());
    assert!(!second_token_path.exists());

    let durable = root
        .join(".aster/logs")
        .join(root.file_name().unwrap())
        .join("edge/logs.txt");
    assert!(fs::read_to_string(durable)
        .unwrap()
        .contains("TLS edge 'edge' ready"));
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
    https_get_with_roots(port, sni, host, vec![cert])
}

fn assert_tls_handshake_rejected(port: u16, sni: &str, cert: Vec<u8>) {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(cert)).unwrap();
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
    assert!(
        tls.write_all(b"GET / HTTP/1.1\r\nHost: outside.invalid\r\nConnection: close\r\n\r\n")
            .is_err(),
        "unrouted SNI completed a TLS handshake"
    );
}

fn https_get_with_roots(
    port: u16,
    sni: &str,
    host: &str,
    certs: Vec<CertificateDer<'static>>,
) -> String {
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).unwrap();
    }
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
