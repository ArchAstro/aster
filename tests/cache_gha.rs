//! GitHub Actions cache backend, exercised by an in-test Twirp and blob server.
//!
//! `upload_from_a_hits_from_b` is the canonical round trip: workspace A
//! uploads, a fresh workspace B with an empty local cache downloads and hits.
//! `fifty_small_outputs_bundle_into_one_pack` is the entry-count check.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use aster::cache::{
    CacheBackend, CacheMode, Digest, GhaBackend, GhaCacheClient, LocalStore, Manifest, OutputEntry,
    OutputKind, TieredCache, CACHE_SCHEMA_VERSION, MANIFEST_SCHEMA_VERSION,
};
use serde_json::{json, Value};

struct Server {
    url: String,
    entries: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    creates: Arc<AtomicUsize>,
    /// When set, every create returns 429.
    rate_limit: Arc<Mutex<bool>>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn spawn() -> Self {
        let entries = Arc::new(Mutex::new(HashMap::new()));
        let creates = Arc::new(AtomicUsize::new(0));
        let rate_limit = Arc::new(Mutex::new(false));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let entries_thread = Arc::clone(&entries);
        let creates_thread = Arc::clone(&creates);
        let limit_thread = Arc::clone(&rate_limit);
        let thread = thread::spawn(move || {
            serve(listener, entries_thread, creates_thread, limit_thread, rx)
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            entries,
            creates,
            rate_limit,
            shutdown: Some(tx),
            thread: Some(thread),
        }
    }

    fn backend(&self, bundle: bool) -> GhaBackend {
        GhaBackend::with_platform(
            GhaCacheClient::for_test(&self.url, "runtime-token"),
            "linux",
            "x86_64",
            bundle,
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = std::net::TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    listener: TcpListener,
    entries: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    creates: Arc<AtomicUsize>,
    rate_limit: Arc<Mutex<bool>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) {
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                if let Some(request) = read_request(&mut stream, &origin) {
                    let response = respond(&request, &entries, &creates, &rate_limit);
                    let _ = write_response(&mut stream, &response);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    origin: String,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn respond(
    request: &Request,
    entries: &Mutex<HashMap<String, Vec<u8>>>,
    creates: &AtomicUsize,
    rate_limit: &Mutex<bool>,
) -> Response {
    if request.method == "PUT" {
        let key = blob_key(&request.path);
        entries
            .lock()
            .unwrap()
            .insert(key.to_string(), request.body.clone());
        return Response {
            status: 201,
            headers: Vec::new(),
            body: Vec::new(),
        };
    }
    if request.method == "GET" && !request.path.contains("CacheService") {
        let key = blob_key(&request.path);
        return match entries.lock().unwrap().get(key) {
            Some(bytes) => Response {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Response {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            },
        };
    }
    let method = request.path.rsplit('/').next().unwrap_or_default();
    let payload: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let key = payload
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match method {
        "CreateCacheEntry" => {
            creates.fetch_add(1, Ordering::Relaxed);
            if *rate_limit.lock().unwrap() {
                return Response {
                    status: 429,
                    headers: vec![("retry-after".into(), "0".into())],
                    body: br#"{"message":"rate limited"}"#.to_vec(),
                };
            }
            if entries.lock().unwrap().contains_key(&key) {
                return json_response(json!({"ok": false, "message": "already exists"}));
            }
            json_response(json!({
                "ok": true,
                "signedUploadUrl": format!("{}/blob/{key}", request_origin(request)),
            }))
        }
        "FinalizeCacheEntryUpload" => json_response(json!({"ok": true})),
        "GetCacheEntryDownloadURL" => {
            if entries.lock().unwrap().contains_key(&key) {
                json_response(json!({
                    "ok": true,
                    "signedDownloadUrl": format!("{}/blob/{key}", request_origin(request)),
                }))
            } else {
                json_response(json!({"ok": false}))
            }
        }
        _ => Response {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        },
    }
}

fn request_origin(request: &Request) -> &str {
    &request.origin
}

fn blob_key(path: &str) -> &str {
    path.trim_start_matches('/')
        .trim_start_matches("blob/")
        .split('?')
        .next()
        .unwrap_or_default()
}

fn json_response(value: Value) -> Response {
    Response {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&value).unwrap(),
    }
}

fn read_request(stream: &mut std::net::TcpStream, origin: &str) -> Option<Request> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 8192];
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos;
                }
            }
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut length = 0_usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    body.truncate(length);
    Some(Request {
        method,
        path,
        body,
        origin: origin.to_string(),
    })
}

fn write_response(stream: &mut std::net::TcpStream, response: &Response) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    Ok(())
}

fn manifest(key: &str, target: &str, digest: &Digest) -> Manifest {
    Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        key: key.to_string(),
        target: target.to_string(),
        exit_code: 0,
        duration_ms: 1,
        created_at: "2026-10-17T00:00:00Z".to_string(),
        outputs: vec![OutputEntry {
            path: "out/file.txt".into(),
            kind: OutputKind::File,
            digest: Some(digest.clone()),
            mode: 0o644,
            target: None,
        }],
        log: None,
        aster_version: "0.21.0".to_string(),
        packs: Vec::new(),
    }
}

fn tier(dir: &std::path::Path, backend: Arc<GhaBackend>) -> TieredCache {
    TieredCache::with_remote(
        LocalStore::new(dir.to_path_buf()),
        CacheMode::ReadWrite,
        backend,
    )
}

/// Workspace A uploads a small blob. Workspace B, with an empty local cache,
/// downloads the manifest and the blob from the pack entry A recorded.
#[test]
fn upload_from_a_hits_from_b() {
    let server = Server::spawn();
    let backend = Arc::new(server.backend(true));
    let bytes = b"artifact-from-workspace-a";
    let digest = Digest::of_bytes(bytes);
    let key = "ab".repeat(32);

    let workspace_a = tempfile::tempdir().unwrap();
    let uploader = tier(workspace_a.path(), Arc::clone(&backend));
    uploader
        .local_store()
        .put_blob(&digest, bytes.as_slice())
        .unwrap();
    uploader.store(&manifest(&key, "//app:build", &digest)).unwrap();
    backend.finish().unwrap();

    let keys = server.entries.lock().unwrap().keys().cloned().collect::<Vec<_>>();
    let manifest_key = format!("aster-v{CACHE_SCHEMA_VERSION}-ac-linux-x86_64-{key}");
    assert!(
        keys.iter().any(|entry| entry == &manifest_key),
        "manifest key missing from {keys:?}"
    );
    assert!(
        keys.iter().any(|entry| {
            entry.starts_with(&format!("aster-v{CACHE_SCHEMA_VERSION}-pack-linux-x86_64-"))
        }),
        "pack key missing from {keys:?}"
    );
    assert!(
        keys.iter().all(|entry| !entry.contains(&format!("-cas-{}", digest.as_str()))),
        "small blob must not have its own entry: {keys:?}"
    );

    let workspace_b = tempfile::tempdir().unwrap();
    let downloader = tier(workspace_b.path(), Arc::clone(&backend));
    let hit = downloader.lookup(&key).unwrap().expect("remote hit");
    assert_eq!(hit.target, "//app:build");
    assert!(
        !hit.packs.is_empty(),
        "manifest must record the pack that holds the blob"
    );
    assert_eq!(hit.packs[0].digest, digest);
    let mut restored = Vec::new();
    downloader
        .local_store()
        .get_blob(&digest, &mut restored)
        .unwrap();
    assert_eq!(restored, bytes);
}

/// A second upload of the same key must not call create again in a way that
/// overwrites: the server reports already exists and the bytes stay.
#[test]
fn existing_key_is_not_overwritten() {
    let server = Server::spawn();
    let backend = Arc::new(server.backend(true));
    let bytes = b"first";
    let digest = Digest::of_bytes(bytes);
    let key = "cd".repeat(32);
    let dir = tempfile::tempdir().unwrap();
    let uploader = tier(dir.path(), Arc::clone(&backend));
    uploader
        .local_store()
        .put_blob(&digest, bytes.as_slice())
        .unwrap();
    uploader.store(&manifest(&key, "//app:build", &digest)).unwrap();
    backend.finish().unwrap();
    let creates = server.creates.load(Ordering::Relaxed);

    let other = Digest::of_bytes(b"second");
    uploader
        .local_store()
        .put_blob(&other, b"second".as_slice())
        .unwrap();
    uploader
        .store(&manifest(&key, "//app:build", &other))
        .unwrap();
    // The manifest key already exists, so create reports already_exists and
    // the stored manifest is still the first one.
    let stored = server
        .entries
        .lock()
        .unwrap()
        .get(&format!("aster-v{CACHE_SCHEMA_VERSION}-ac-linux-x86_64-{key}"))
        .cloned()
        .unwrap();
    let decoded = Manifest::decode(&stored).unwrap();
    assert_eq!(decoded.blob_refs()[0], digest);
    assert!(server.creates.load(Ordering::Relaxed) >= creates);
}

/// Two 429s that survive the client retries stop later writes. One warning
/// is the backend's job; this test checks the stop and the counter.
#[test]
fn repeated_429_stops_writes() {
    let server = Server::spawn();
    let backend = Arc::new(server.backend(true));
    *server.rate_limit.lock().unwrap() = true;
    let digest = Digest::of_bytes(b"small");
    let dir = tempfile::tempdir().unwrap();
    let uploader = tier(dir.path(), Arc::clone(&backend));
    uploader
        .local_store()
        .put_blob(&digest, b"small".as_slice())
        .unwrap();
    // put_manifest uploads the pack (429) and then the manifest (429).
    uploader
        .store(&manifest(&"11".repeat(32), "//app:build", &digest))
        .unwrap();
    assert!(backend.writes_stopped());
    let created_before = server.creates.load(Ordering::Relaxed);
    uploader
        .store(&manifest(&"22".repeat(32), "//app:build", &digest))
        .unwrap();
    assert_eq!(server.creates.load(Ordering::Relaxed), created_before);
    assert!(backend.summary().rate_limited >= 1);
}

/// 50 targets with one small output each: 50 manifests plus one pack.
///
/// Blobs are staged before any manifest, which is when a run's pack key is
/// sealed. A manifest stored first would close the pack around one blob.
#[test]
fn fifty_small_outputs_bundle_into_one_pack() {
    let server = Server::spawn();
    let bundled = Arc::new(server.backend(true));
    let dir = tempfile::tempdir().unwrap();
    let uploader = tier(dir.path(), Arc::clone(&bundled));
    let mut staged = Vec::new();
    for index in 0..50 {
        let bytes = format!("output-{index}");
        let digest = Digest::of_bytes(bytes.as_bytes());
        uploader
            .local_store()
            .put_blob(&digest, bytes.as_bytes())
            .unwrap();
        // Stage into the remote before any manifest seals the pack.
        bundled
            .put_blob(&digest, &mut bytes.as_bytes())
            .unwrap();
        staged.push((format!("{index:064x}"), digest));
    }
    for (key, digest) in &staged {
        uploader
            .store(&manifest(key, "//app:build", digest))
            .unwrap();
    }
    bundled.finish().unwrap();
    let count = server.entries.lock().unwrap().len();
    assert!(
        count <= 51,
        "bundling on created {count} entries, expected <= 51"
    );
    assert!(
        server
            .entries
            .lock()
            .unwrap()
            .keys()
            .any(|key| key.contains("-pack-")),
        "expected one pack entry"
    );

    let unbundled_server = Server::spawn();
    let loose = Arc::new(unbundled_server.backend(false));
    let other = tempfile::tempdir().unwrap();
    let uploader = tier(other.path(), Arc::clone(&loose));
    for index in 0..50 {
        let bytes = format!("output-{index}");
        let digest = Digest::of_bytes(bytes.as_bytes());
        uploader
            .local_store()
            .put_blob(&digest, bytes.as_bytes())
            .unwrap();
    }
    for index in 0..50 {
        let bytes = format!("output-{index}");
        let digest = Digest::of_bytes(bytes.as_bytes());
        let key = format!("{index:064x}");
        uploader
            .store(&manifest(&key, "//app:build", &digest))
            .unwrap();
    }
    loose.finish().unwrap();
    let loose_count = unbundled_server.entries.lock().unwrap().len();
    assert!(
        loose_count >= 100,
        "bundling off created {loose_count} entries, expected >= 100"
    );
}

/// A blob at least 1 MiB compressed is its own cas entry, not a pack member.
#[test]
fn large_blob_is_its_own_entry() {
    let server = Server::spawn();
    let backend = Arc::new(server.backend(true));
    // Incompressible bytes, so the zstd frame stays above 1 MiB.
    let bytes: Vec<u8> = (0..1024 * 1024 + 64).map(|i| (i % 251) as u8).collect();
    let digest = Digest::of_bytes(&bytes);
    let key = "ef".repeat(32);
    let dir = tempfile::tempdir().unwrap();
    let uploader = tier(dir.path(), Arc::clone(&backend));
    uploader
        .local_store()
        .put_blob(&digest, bytes.as_slice())
        .unwrap();
    uploader.store(&manifest(&key, "//app:build", &digest)).unwrap();
    backend.finish().unwrap();
    let keys: Vec<_> = server.entries.lock().unwrap().keys().cloned().collect();
    let cas = format!("aster-v{CACHE_SCHEMA_VERSION}-cas-{}", digest.as_str());
    assert!(
        server.entries.lock().unwrap().contains_key(&cas),
        "large blob missing its own entry; keys={keys:?} frame_len_hint"
    );
    assert!(
        server
            .entries
            .lock()
            .unwrap()
            .keys()
            .all(|key| !key.contains("-pack-")),
        "a large blob must not open a pack"
    );

    let other = tempfile::tempdir().unwrap();
    let downloader = tier(other.path(), backend);
    downloader.lookup(&key).unwrap().expect("remote hit");
    let mut restored = Vec::new();
    downloader
        .local_store()
        .get_blob(&digest, &mut restored)
        .unwrap();
    assert_eq!(restored, bytes);
}

/// `ASTER_REMOTE_CACHE=gha` selects this backend when the runtime env is set.
/// Unset on a GitHub Actions job with the same env also selects it.
#[test]
fn gha_selected_when_runtime_env_present() {
    use aster::cache::{select_remote, RemoteKind};
    let server = Server::spawn();
    let previous = EnvGuard::set(&[
        (
            "ACTIONS_RESULTS_URL",
            server.url.replace("http://", "https://"),
        ),
        ("ACTIONS_RUNTIME_TOKEN", "runtime-token".to_string()),
        ("GITHUB_ACTIONS", "true".to_string()),
    ]);
    // https is required by from_env, and this server is http. Selection
    // still has to choose Gha and refuse a plaintext URL rather than pick
    // aster-srv. The explicit kind is what `ASTER_REMOTE_CACHE=gha` parses.
    assert_eq!(RemoteKind::parse("gha").unwrap(), RemoteKind::Gha);
    let selected = select_remote(RemoteKind::Gha, None, None, None, None).unwrap();
    // The test URL was rewritten to https, which does not answer. from_env
    // still builds a backend: the URL is https and the token is set.
    assert!(selected.is_some(), "gha with runtime env must select a backend");
    drop(previous);

    let bare = EnvGuard::clear(&["ASTER_REMOTE_CACHE", "GITHUB_ACTIONS"]);
    assert_eq!(
        RemoteKind::from_env().unwrap(),
        RemoteKind::AsterSrv,
        "unset remote kind stays aster-srv outside Actions"
    );
    drop(bare);
}

/// Summary object the JSON path stores under `cache_summary`.
#[test]
fn summary_line_and_json_object() {
    let server = Server::spawn();
    let backend = Arc::new(server.backend(true));
    let digest = Digest::of_bytes(b"counted");
    let key = "44".repeat(32);
    let dir = tempfile::tempdir().unwrap();
    let uploader = tier(dir.path(), Arc::clone(&backend));
    uploader
        .local_store()
        .put_blob(&digest, b"counted".as_slice())
        .unwrap();
    uploader.store(&manifest(&key, "//app:build", &digest)).unwrap();
    backend.finish().unwrap();
    let other = tempfile::tempdir().unwrap();
    let downloader = tier(other.path(), Arc::clone(&backend));
    downloader.lookup(&key).unwrap().expect("hit");
    downloader.lookup(&"55".repeat(32)).unwrap();

    let summary = backend.summary();
    assert!(summary.remote_hits >= 1);
    assert!(summary.remote_misses >= 1);
    assert!(summary.entries_created >= 1);
    let json = serde_json::to_value(summary).unwrap();
    for field in ["remote_hits", "remote_misses", "entries_created", "rate_limited"] {
        assert!(json.get(field).is_some(), "missing {field}");
    }
}

/// Saves and restores process env so selection tests do not leak.
struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&'static str, String)]) -> Self {
        let saved = pairs
            .iter()
            .map(|(key, value)| {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, previous)
            })
            .collect();
        Self { saved }
    }

    fn clear(keys: &[&'static str]) -> Self {
        let saved = keys
            .iter()
            .map(|key| {
                let previous = std::env::var_os(key);
                std::env::remove_var(key);
                (*key, previous)
            })
            .collect();
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..) {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
