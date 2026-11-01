//! Remote cache through an in-test aster-srv-shaped gateway.
//!
//! The server speaks `POST /v1/cache/lookup|upload|commit` and hands blob
//! bytes to a fake object store over the signed URLs it returns. Checkout A
//! uploads; a clean checkout B restores without running the target.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};

struct Gateway {
    url: String,
    lookups: Arc<AtomicUsize>,
    #[allow(dead_code)]
    uploads: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    puts: Arc<AtomicUsize>,
    /// When set, upload answers 403, as a pull-request identity would.
    upload_forbidden: Arc<AtomicBool>,
    /// When set, commit answers with this status and stores nothing.
    commit_status: Arc<Mutex<u16>>,
    /// When set, every blob GET returns these bytes instead of the upload.
    poison: Arc<Mutex<Option<Vec<u8>>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Gateway {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let lookups = Arc::new(AtomicUsize::new(0));
        let uploads = Arc::new(AtomicUsize::new(0));
        let commits = Arc::new(AtomicUsize::new(0));
        let puts = Arc::new(AtomicUsize::new(0));
        let upload_forbidden = Arc::new(AtomicBool::new(false));
        let commit_status = Arc::new(Mutex::new(201_u16));
        let poison = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let objects = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let manifests = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let base = url.clone();
        let thread = thread::spawn({
            let lookups = Arc::clone(&lookups);
            let uploads = Arc::clone(&uploads);
            let commits = Arc::clone(&commits);
            let puts = Arc::clone(&puts);
            let upload_forbidden = Arc::clone(&upload_forbidden);
            let commit_status = Arc::clone(&commit_status);
            let poison = Arc::clone(&poison);
            let shutdown = Arc::clone(&shutdown);
            move || {
                serve(
                    listener,
                    &base,
                    &lookups,
                    &uploads,
                    &commits,
                    &puts,
                    &upload_forbidden,
                    &commit_status,
                    &objects,
                    &manifests,
                    &poison,
                    &shutdown,
                );
            }
        });
        Self {
            url,
            lookups,
            uploads,
            commits,
            puts,
            upload_forbidden,
            commit_status,
            poison,
            shutdown,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Unblock accept.
        let _ = std::net::TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: TcpListener,
    base: &str,
    lookups: &AtomicUsize,
    uploads: &AtomicUsize,
    commits: &AtomicUsize,
    puts: &AtomicUsize,
    upload_forbidden: &AtomicBool,
    commit_status: &Mutex<u16>,
    objects: &Mutex<HashMap<String, Vec<u8>>>,
    manifests: &Mutex<HashMap<String, Vec<u8>>>,
    poison: &Mutex<Option<Vec<u8>>>,
    shutdown: &AtomicBool,
) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let Ok(mut stream) = stream else { break };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(&stream);
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() || request.is_empty() {
            continue;
        }
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0_u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).is_err() {
            continue;
        }
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let (status, payload, json_body) = dispatch(
            method,
            path,
            &body,
            base,
            lookups,
            uploads,
            commits,
            puts,
            upload_forbidden,
            commit_status,
            objects,
            manifests,
            poison,
        );
        let content_type = if json_body {
            "application/json"
        } else {
            "application/octet-stream"
        };
        let header = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&payload);
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    method: &str,
    path: &str,
    body: &[u8],
    base: &str,
    lookups: &AtomicUsize,
    uploads: &AtomicUsize,
    commits: &AtomicUsize,
    puts: &AtomicUsize,
    upload_forbidden: &AtomicBool,
    commit_status: &Mutex<u16>,
    objects: &Mutex<HashMap<String, Vec<u8>>>,
    manifests: &Mutex<HashMap<String, Vec<u8>>>,
    poison: &Mutex<Option<Vec<u8>>>,
) -> (u16, Vec<u8>, bool) {
    if let Some(digest) = path.strip_prefix("/objects/") {
        return object(method, digest, body, puts, objects, poison);
    }
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match path {
        "/v1/cache/lookup" => {
            lookups.fetch_add(1, Ordering::Relaxed);
            let mut hits = serde_json::Map::new();
            let stored = manifests.lock().unwrap();
            let blobs = objects.lock().unwrap();
            for key in request["keys"].as_array().into_iter().flatten() {
                let Some(key) = key.as_str() else { continue };
                let Some(bytes) = stored.get(key) else {
                    continue;
                };
                let mut urls = serde_json::Map::new();
                if let Ok(manifest) = serde_json::from_slice::<Value>(bytes) {
                    for digest in blob_names(&manifest) {
                        if blobs.contains_key(&digest) {
                            urls.insert(
                                digest.clone(),
                                Value::String(format!("{base}/objects/{digest}")),
                            );
                        }
                    }
                }
                hits.insert(
                    key.to_string(),
                    json!({
                        "manifest": BASE64.encode(bytes),
                        "blobs": urls,
                    }),
                );
            }
            (
                200,
                serde_json::to_vec(&json!({"hits": hits})).unwrap(),
                true,
            )
        }
        "/v1/cache/upload" => {
            uploads.fetch_add(1, Ordering::Relaxed);
            if upload_forbidden.load(Ordering::Relaxed) {
                return (
                    403,
                    serde_json::to_vec(&json!({"error": "read only"})).unwrap(),
                    true,
                );
            }
            let mut missing = serde_json::Map::new();
            let blobs = objects.lock().unwrap();
            for digest in request["digests"].as_array().into_iter().flatten() {
                let Some(digest) = digest.as_str() else {
                    continue;
                };
                if !blobs.contains_key(digest) {
                    missing.insert(
                        digest.to_string(),
                        Value::String(format!("{base}/objects/{digest}")),
                    );
                }
            }
            (
                200,
                serde_json::to_vec(&json!({"missing": missing})).unwrap(),
                true,
            )
        }
        "/v1/cache/commit" => {
            commits.fetch_add(1, Ordering::Relaxed);
            let status = *commit_status.lock().unwrap();
            if status != 201 {
                return (
                    status,
                    serde_json::to_vec(&json!({"error": "refused"})).unwrap(),
                    true,
                );
            }
            let key = request["key"].as_str().unwrap_or("");
            let encoded = request["manifest"].as_str().unwrap_or("");
            let bytes = BASE64.decode(encoded).unwrap_or_default();
            manifests.lock().unwrap().insert(key.to_string(), bytes);
            (201, br#"{"stored":true}"#.to_vec(), true)
        }
        _ => (404, br#"{"error":"not found"}"#.to_vec(), true),
    }
}

fn object(
    method: &str,
    digest: &str,
    body: &[u8],
    puts: &AtomicUsize,
    objects: &Mutex<HashMap<String, Vec<u8>>>,
    poison: &Mutex<Option<Vec<u8>>>,
) -> (u16, Vec<u8>, bool) {
    if method == "PUT" {
        puts.fetch_add(1, Ordering::Relaxed);
        objects
            .lock()
            .unwrap()
            .insert(digest.to_string(), body.to_vec());
        return (200, Vec::new(), false);
    }
    if let Some(bytes) = poison.lock().unwrap().clone() {
        return (200, bytes, false);
    }
    match objects.lock().unwrap().get(digest) {
        Some(bytes) => (200, bytes.clone(), false),
        None => (404, br#"{"error":"missing"}"#.to_vec(), true),
    }
}

/// Digests named by a manifest, without depending on the crate's private types.
fn blob_names(manifest: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(outputs) = manifest["outputs"].as_array() {
        for output in outputs {
            if let Some(digest) = output["digest"].as_str() {
                names.push(digest.to_string());
            }
        }
    }
    if let Some(log) = manifest["log"].as_str() {
        names.push(log.to_string());
    }
    names
}

fn checkout(dir: &Path) {
    std::fs::create_dir_all(dir.join("app")).unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    // Project-relative counter, so two checkouts hash the same command.
    std::fs::write(
        dir.join("app/aster.toml"),
        r#"
[targets.build]
command = "bash -c 'mkdir -p out && echo hi > out/file.txt && echo ran >> runs.txt'"
# runs.txt only counts executions. It is not an input: hashing it would
# make the post-run key differ, so the result would never be stored.
cache = { enabled = true, outputs = ["out/**"], exclude = ["runs.txt"] }
"#,
    )
    .unwrap();
}

fn run(dir: &Path, gateway: &str) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(dir)
        .env("ASTER_SRV_URL", gateway)
        .env("ASTER_SRV_TOKEN", "test-token")
        .env("GITHUB_REPOSITORY", "acme/app")
        .env_remove("ASTER_REMOTE_CACHE")
        .env_remove("ASTER_CACHE_MODE")
        .args(["run", "//app:build", "--no-deps"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "aster failed in {}: stdout={} stderr={}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("app/runs.txt"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn remote_hit_restores_without_running() {
    let gateway = Gateway::spawn();
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    checkout(&a);
    checkout(&b);

    let first = run(&a, &gateway.url);
    assert_eq!(
        runs(&a),
        1,
        "checkout A must run the target once: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(std::fs::read(a.join("app/out/file.txt")).unwrap(), b"hi\n");
    assert!(
        gateway.commits.load(Ordering::Relaxed) >= 1,
        "A must commit"
    );
    assert!(
        gateway.puts.load(Ordering::Relaxed) >= 1,
        "A must upload a blob"
    );

    let _output = run(&b, &gateway.url);
    assert_eq!(
        std::fs::read(b.join("app/out/file.txt")).unwrap(),
        b"hi\n",
        "checkout B must restore the uploaded output"
    );
    assert_eq!(runs(&b), 0, "checkout B must not run the target");
    assert_eq!(
        gateway.commits.load(Ordering::Relaxed),
        1,
        "a hit must not commit another manifest"
    );
    assert!(!format!(
        "{}{}",
        String::from_utf8_lossy(&std::fs::read(b.join("app/out/file.txt")).unwrap_or_default()),
        ""
    )
    .contains("test-token"));
}

#[test]
fn read_only_lookup_does_not_call_upload() {
    let gateway = Gateway::spawn();
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    checkout(&a);
    checkout(&b);
    run(&a, &gateway.url);
    let uploads_after_write = gateway.uploads.load(Ordering::Relaxed);
    gateway.upload_forbidden.store(true, Ordering::Relaxed);

    run(&b, &gateway.url);
    assert_eq!(runs(&b), 0, "a reader restores without running");
    assert_eq!(
        gateway.uploads.load(Ordering::Relaxed),
        uploads_after_write,
        "a lookup must not call /upload"
    );
}

#[test]
fn lookup_is_batched_per_level() {
    let gateway = Gateway::spawn();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("ws");
    checkout(&dir);
    // A second independent target at the same level.
    std::fs::write(
        dir.join("app/aster.toml"),
        format!(
            r#"
[targets.build]
command = "bash -c 'mkdir -p out && echo hi > out/file.txt && echo ran >> {}'"
cache = {{ enabled = true, outputs = ["out/**"] }}

[targets.lint]
command = "bash -c 'echo lint >> {}'"
# Build's declared output is not a lint input. Without this exclude, restoring
# out/ changes lint's key after the shared lookup and forces a second one.
cache = {{ enabled = true, outputs = [], exclude = ["out/**"] }}
"#,
            root.path().join("runs.txt").display(),
            root.path().join("runs.txt").display(),
        ),
    )
    .unwrap();

    let first_out = run_targets(&dir, &gateway.url, &["//app:build", "//app:lint"]);
    let first = gateway.lookups.load(Ordering::Relaxed);
    assert!(
        first >= 1,
        "the level must look up at least once: {}",
        String::from_utf8_lossy(&first_out.stderr)
    );

    std::fs::remove_dir_all(dir.join("app/out")).ok();
    std::fs::remove_dir_all(dir.join(".aster")).ok();
    let second_out = run_targets(&dir, &gateway.url, &["//app:build", "//app:lint"]);
    let second = gateway.lookups.load(Ordering::Relaxed) - first;
    assert_eq!(
        second,
        1,
        "two targets at one level share one lookup, got {second}: {}",
        String::from_utf8_lossy(&second_out.stderr)
    );
}

fn run_targets(dir: &Path, gateway: &str, targets: &[&str]) -> std::process::Output {
    let mut args = vec!["run"];
    args.extend_from_slice(targets);
    args.push("--no-deps");
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(dir)
        .env("ASTER_SRV_URL", gateway)
        .env("ASTER_SRV_TOKEN", "test-token")
        .env("GITHUB_REPOSITORY", "acme/app")
        .env_remove("ASTER_REMOTE_CACHE")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "aster failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn corrupt_blob_is_a_miss() {
    let gateway = Gateway::spawn();
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    checkout(&a);
    checkout(&b);

    run(&a, &gateway.url);
    *gateway.poison.lock().unwrap() = Some(b"not-the-uploaded-bytes".to_vec());

    run(&b, &gateway.url);
    assert_eq!(
        runs(&b),
        1,
        "a digest mismatch must run the target instead of restoring"
    );
    assert_eq!(
        std::fs::read(b.join("app/out/file.txt")).unwrap(),
        b"hi\n",
        "the miss still produces the output by running"
    );
}

#[test]
fn commit_forbidden_degrades_to_read() {
    let gateway = Gateway::spawn();
    *gateway.commit_status.lock().unwrap() = 403;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("ws");
    checkout(&dir);

    let output = run(&dir, &gateway.url);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("read-only"),
        "a 403 commit must say the cache is read-only: {stderr}"
    );
    assert!(
        !stderr.contains("test-token"),
        "the token must not be logged: {stderr}"
    );
    assert_eq!(runs(&dir), 1);
    assert_eq!(gateway.commits.load(Ordering::Relaxed), 1);

    // A second target run in the same process isn't available; a second
    // invocation starts read-write again and is refused once more. The
    // in-process switch is covered by the unit test. Here, the build still
    // succeeds and the local output is present.
    assert_eq!(
        std::fs::read(dir.join("app/out/file.txt")).unwrap(),
        b"hi\n"
    );
}

#[test]
fn server_down_does_not_fail_the_build() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("ws");
    checkout(&dir);
    // Nothing is listening here.
    let output = run(&dir, "http://127.0.0.1:9");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("test-token"),
        "the token must not be logged: {stderr}"
    );
    assert_eq!(runs(&dir), 1, "the target still runs");
    assert_eq!(
        std::fs::read(dir.join("app/out/file.txt")).unwrap(),
        b"hi\n"
    );
}

#[test]
fn slow_server_does_not_stall_the_build() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            thread::sleep(Duration::from_secs(45));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("ws");
    checkout(&dir);
    let started_at = std::time::Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(&dir)
        .env("ASTER_SRV_URL", format!("http://{addr}"))
        .env("ASTER_SRV_TOKEN", "test-token")
        .env("GITHUB_REPOSITORY", "acme/app")
        .args(["run", "//app:build", "--no-deps"])
        .output()
        .unwrap();
    let elapsed = started_at.elapsed();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        elapsed < Duration::from_secs(20),
        "a slow gateway must not stall the build, took {elapsed:?}: {stderr}"
    );
    assert_eq!(runs(&dir), 1, "the target still runs: {stderr}");
    assert!(
        !stderr.contains("test-token"),
        "the token must not be logged: {stderr}"
    );
}
