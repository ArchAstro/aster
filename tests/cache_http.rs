//! HTTP remote cache: bazel-remote layout, exercised by an in-process server.
//!
//! `upload_from_a_hits_from_b` is the canonical round trip. `put_403_degrades_to_read_only`
//! and `put_409_is_success` cover the status rules. `bazel_remote_upload_from_a_hits_from_b`
//! is ignored unless `ASTER_BAZEL_REMOTE_URL` points at a running bazel-remote.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use aster::cache::{
    CacheBackend, CacheMode, Digest, HttpCache, LocalStore, Manifest, OutputEntry, OutputKind,
    TieredCache, MANIFEST_SCHEMA_VERSION,
};

const TOKEN: &str = "test-token-do-not-log";

struct Server {
    url: String,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Paths whose PUT returns this status instead of storing.
    put_status: Arc<Mutex<HashMap<String, u16>>>,
    seen_auth: Arc<Mutex<Vec<String>>>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn spawn() -> Self {
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let put_status = Arc::new(Mutex::new(HashMap::new()));
        let seen_auth = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let objects_thread = Arc::clone(&objects);
        let status_thread = Arc::clone(&put_status);
        let auth_thread = Arc::clone(&seen_auth);
        let thread =
            thread::spawn(move || serve(listener, objects_thread, status_thread, auth_thread, rx));
        Self {
            url: format!("http://127.0.0.1:{port}"),
            objects,
            put_status,
            seen_auth,
            shutdown: Some(tx),
            thread: Some(thread),
        }
    }

    fn refuse_put(&self, path: &str, status: u16) {
        self.put_status
            .lock()
            .unwrap()
            .insert(path.to_string(), status);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    listener: TcpListener,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    put_status: Arc<Mutex<HashMap<String, u16>>>,
    seen_auth: Arc<Mutex<Vec<String>>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) {
    listener.set_nonblocking(true).unwrap();
    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                if let Some(request) = read_request(&mut stream) {
                    let response = respond(&request, &objects, &put_status);
                    seen_auth.lock().unwrap().push(request.authorization);
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
    authorization: String,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

fn respond(
    request: &Request,
    objects: &Mutex<HashMap<String, Vec<u8>>>,
    put_status: &Mutex<HashMap<String, u16>>,
) -> Response {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    match request.method.as_str() {
        "GET" => match objects.lock().unwrap().get(path) {
            Some(body) => Response {
                status: 200,
                body: body.clone(),
            },
            None => Response {
                status: 404,
                body: Vec::new(),
            },
        },
        "HEAD" => Response {
            status: if objects.lock().unwrap().contains_key(path) {
                200
            } else {
                404
            },
            body: Vec::new(),
        },
        "PUT" => {
            if let Some(status) = put_status.lock().unwrap().get(path).copied() {
                return Response {
                    status,
                    body: b"refused".to_vec(),
                };
            }
            let mut objects = objects.lock().unwrap();
            if objects.contains_key(path) {
                return Response {
                    status: 409,
                    body: Vec::new(),
                };
            }
            objects.insert(path.to_string(), request.body.clone());
            Response {
                status: 200,
                body: Vec::new(),
            }
        }
        _ => Response {
            status: 405,
            body: Vec::new(),
        },
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
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
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let authorization = headers.get("authorization").cloned().unwrap_or_default();
    let mut body = buf[header_end + 4..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        body = read_chunked(stream, &body)?;
    } else {
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while body.len() < length {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        body.truncate(length);
    }
    Some(Request {
        method,
        path,
        authorization,
        body,
    })
}

fn read_chunked(stream: &mut TcpStream, already: &[u8]) -> Option<Vec<u8>> {
    let mut raw = already.to_vec();
    let mut tmp = [0_u8; 8192];
    let mut decoded = Vec::new();
    loop {
        let line_end = loop {
            if let Some(pos) = raw.windows(2).position(|window| window == b"\r\n") {
                break pos;
            }
            match stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => raw.extend_from_slice(&tmp[..n]),
                Err(_) => return None,
            }
        };
        let size_line = std::str::from_utf8(&raw[..line_end]).ok()?;
        let size = usize::from_str_radix(size_line.trim().split(';').next()?, 16).ok()?;
        raw.drain(..line_end + 2);
        while raw.len() < size + 2 {
            match stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => raw.extend_from_slice(&tmp[..n]),
                Err(_) => return None,
            }
        }
        if size == 0 {
            return Some(decoded);
        }
        decoded.extend_from_slice(&raw[..size]);
        raw.drain(..size + 2);
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    Ok(())
}

fn manifest(key: &str, digest: &Digest) -> Manifest {
    Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        key: key.to_string(),
        target: "//app:build".to_string(),
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
    }
}

fn client(server: &Server) -> HttpCache {
    HttpCache::new(&server.url, Some(TOKEN.to_string())).unwrap()
}

#[test]
fn upload_from_a_hits_from_b() {
    let server = Server::spawn();
    let bytes = b"artifact-from-a";
    let digest = Digest::of_bytes(bytes);
    let key = "ab".repeat(32);
    let stored = manifest(&key, &digest);

    let writer = client(&server);
    writer.put_blob(&digest, &mut bytes.as_slice()).unwrap();
    writer.put_manifest(&stored).unwrap();

    let reader = client(&server);
    let mut downloaded = Vec::new();
    assert!(reader.get_blob(&digest, &mut downloaded).unwrap());
    assert_eq!(downloaded, bytes);
    let hit = reader.get_manifest(&key).unwrap().expect("hit");
    assert_eq!(hit, stored);
    assert!(reader
        .find_missing(std::slice::from_ref(&digest))
        .unwrap()
        .is_empty());

    let auth = server.seen_auth.lock().unwrap().clone();
    assert!(auth.iter().all(|value| value == &format!("Bearer {TOKEN}")));
    assert!(!format!("{writer:?}").contains(TOKEN));
}

#[test]
fn get_404_is_a_miss() {
    let server = Server::spawn();
    let cache = client(&server);
    let key = "cd".repeat(32);
    assert!(cache.get_manifest(&key).unwrap().is_none());
    let digest = Digest::of_bytes(b"missing");
    let mut sink = Vec::new();
    assert!(!cache.get_blob(&digest, &mut sink).unwrap());
    assert_eq!(cache.find_missing(&[digest]).unwrap().len(), 1);
}

#[test]
fn put_409_is_success() {
    let server = Server::spawn();
    let bytes = b"already-there";
    let digest = Digest::of_bytes(bytes);
    let key = "ef".repeat(32);
    let stored = manifest(&key, &digest);
    let cache = client(&server);
    cache.put_blob(&digest, &mut bytes.as_slice()).unwrap();
    cache.put_manifest(&stored).unwrap();

    cache.put_blob(&digest, &mut bytes.as_slice()).unwrap();
    cache.put_manifest(&stored).unwrap();
    let mut downloaded = Vec::new();
    assert!(cache.get_blob(&digest, &mut downloaded).unwrap());
    assert_eq!(downloaded, bytes);
}

#[test]
fn put_403_degrades_to_read_only() {
    let server = Server::spawn();
    let bytes = b"readable";
    let digest = Digest::of_bytes(bytes);
    let key = "11".repeat(32);
    let stored = manifest(&key, &digest);

    let seeder = client(&server);
    seeder.put_blob(&digest, &mut bytes.as_slice()).unwrap();
    seeder.put_manifest(&stored).unwrap();

    server.refuse_put(&format!("/cas/{}", digest.as_str()), 403);
    server.refuse_put(&format!("/ac/{key}"), 403);
    let readonly = client(&server);
    assert!(!readonly.is_read_only());
    let refusal = readonly
        .put_blob(&digest, &mut bytes.as_slice())
        .expect_err("refused upload is not stored")
        .to_string();
    assert!(!refusal.contains(TOKEN), "{refusal}");
    assert!(readonly.is_read_only());
    let calls = server.seen_auth.lock().unwrap().len();
    readonly.put_manifest(&stored).unwrap();
    readonly
        .put_blob(&Digest::of_bytes(b"later"), &mut b"later".as_slice())
        .unwrap();
    assert_eq!(server.seen_auth.lock().unwrap().len(), calls);

    let mut downloaded = Vec::new();
    assert!(readonly.get_blob(&digest, &mut downloaded).unwrap());
    assert_eq!(downloaded, bytes);
    assert_eq!(readonly.get_manifest(&key).unwrap().unwrap(), stored);
}

#[test]
fn put_401_also_degrades_to_read_only() {
    let server = Server::spawn();
    let digest = Digest::of_bytes(b"nope");
    server.refuse_put(&format!("/cas/{}", digest.as_str()), 401);
    let cache = client(&server);
    cache
        .put_blob(&digest, &mut b"nope".as_slice())
        .expect_err("refused upload is not stored");
    assert!(cache.is_read_only());
}

#[test]
fn large_blob_is_streamed() {
    let server = Server::spawn();
    let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let digest = Digest::of_bytes(&bytes);
    let cache = client(&server);
    cache.put_blob(&digest, &mut bytes.as_slice()).unwrap();
    let mut downloaded = Vec::new();
    assert!(cache.get_blob(&digest, &mut downloaded).unwrap());
    assert_eq!(downloaded, bytes);
    let stored = server
        .objects
        .lock()
        .unwrap()
        .get(&format!("/cas/{}", digest.as_str()))
        .cloned()
        .unwrap();
    assert_eq!(stored, bytes);
}

#[test]
fn tier_upload_from_a_hits_from_b() {
    let server = Server::spawn();
    let bytes = b"tier-bytes";
    let digest = Digest::of_bytes(bytes);
    let key = "22".repeat(32);
    let stored = manifest(&key, &digest);

    let a_dir = tempfile::tempdir().unwrap();
    let a = TieredCache::with_remote(
        LocalStore::new(a_dir.path().to_path_buf()),
        CacheMode::ReadWrite,
        std::sync::Arc::new(client(&server)),
    );
    a.local_store().put_blob(&digest, bytes.as_slice()).unwrap();
    a.store(&stored).unwrap();

    let b_dir = tempfile::tempdir().unwrap();
    let b = TieredCache::with_remote(
        LocalStore::new(b_dir.path().to_path_buf()),
        CacheMode::Read,
        std::sync::Arc::new(client(&server)),
    );
    let hit = b.lookup(&key).unwrap().expect("remote hit");
    assert_eq!(hit, stored);
    let mut restored = Vec::new();
    b.local_store().get_blob(&digest, &mut restored).unwrap();
    assert_eq!(restored, bytes);
}

#[test]
fn tier_403_does_not_publish_a_manifest() {
    let server = Server::spawn();
    let bytes = b"not-uploaded";
    let digest = Digest::of_bytes(bytes);
    let key = "44".repeat(32);
    let stored = manifest(&key, &digest);
    server.refuse_put(&format!("/cas/{}", digest.as_str()), 403);

    let dir = tempfile::tempdir().unwrap();
    let tier = TieredCache::with_remote(
        LocalStore::new(dir.path().to_path_buf()),
        CacheMode::ReadWrite,
        std::sync::Arc::new(client(&server)),
    );
    tier.local_store()
        .put_blob(&digest, bytes.as_slice())
        .unwrap();
    tier.store(&stored).unwrap();

    assert!(
        !server
            .objects
            .lock()
            .unwrap()
            .contains_key(&format!("/ac/{key}")),
        "refused blob must not be followed by a manifest"
    );
}

#[test]
fn env_selects_http_backend() {
    let server = Server::spawn();
    let previous_kind = std::env::var_os("ASTER_REMOTE_CACHE");
    let previous_url = std::env::var_os("ASTER_REMOTE_CACHE_URL");
    let previous_token = std::env::var_os("ASTER_REMOTE_CACHE_TOKEN");
    std::env::set_var("ASTER_REMOTE_CACHE", "http");
    std::env::set_var("ASTER_REMOTE_CACHE_URL", &server.url);
    std::env::set_var("ASTER_REMOTE_CACHE_TOKEN", TOKEN);

    let dir = tempfile::tempdir().unwrap();
    let tier = aster::cache::tier_from_env(dir.path()).unwrap();
    assert!(tier.remote_enabled());

    let bytes = b"from-env";
    let digest = Digest::of_bytes(bytes);
    let key = "33".repeat(32);
    let stored = manifest(&key, &digest);
    tier.local_store()
        .put_blob(&digest, bytes.as_slice())
        .unwrap();
    tier.store(&stored).unwrap();
    assert!(server
        .objects
        .lock()
        .unwrap()
        .contains_key(&format!("/ac/{key}")));

    restore("ASTER_REMOTE_CACHE", previous_kind);
    restore("ASTER_REMOTE_CACHE_URL", previous_url);
    restore("ASTER_REMOTE_CACHE_TOKEN", previous_token);
}

fn restore(name: &str, previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

/// Upload from A and hit from B against a local bazel-remote.
///
/// Ignored by default. Run with `ASTER_BAZEL_REMOTE_URL=http://127.0.0.1:8080
/// cargo test --test cache_http -- --ignored bazel_remote_upload_from_a_hits_from_b`.
/// The server must be started with `--disable_http_ac_validation` only if it
/// rejects `Content-Type: application/json`; current bazel-remote accepts that
/// header without the flag.
#[test]
#[ignore]
fn bazel_remote_upload_from_a_hits_from_b() {
    let url = std::env::var("ASTER_BAZEL_REMOTE_URL").expect("ASTER_BAZEL_REMOTE_URL");
    let token = std::env::var("ASTER_REMOTE_CACHE_TOKEN").ok();
    let writer = HttpCache::new(&url, token.clone()).unwrap();
    let reader = HttpCache::new(&url, token).unwrap();
    let bytes = b"bazel-remote-interop";
    let digest = Digest::of_bytes(bytes);
    let key = Digest::of_bytes(b"aster-http-interop-key")
        .as_str()
        .to_string();
    let stored = manifest(&key, &digest);
    writer.put_blob(&digest, &mut bytes.as_slice()).unwrap();
    writer.put_manifest(&stored).unwrap();
    let mut downloaded = Vec::new();
    assert!(reader.get_blob(&digest, &mut downloaded).unwrap());
    assert_eq!(downloaded, bytes);
    assert_eq!(reader.get_manifest(&key).unwrap().unwrap(), stored);
}

#[test]
fn error_text_does_not_contain_the_token() {
    let cache = HttpCache::new("http://127.0.0.1:1", Some(TOKEN.to_string())).unwrap();
    let error = cache
        .get_manifest(&"ab".repeat(32))
        .expect_err("nothing listens")
        .to_string();
    assert!(!error.contains(TOKEN), "{error}");
    assert!(!format!("{cache:?}").contains(TOKEN));
}
