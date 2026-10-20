//! aster-srv cache client.
//!
//! Speaks `POST /v1/cache/lookup`, `/upload`, and `/commit`. Blob bytes move
//! through the signed URLs those return. Credentials are the ones test-result
//! uploads already use: GitHub OIDC in Actions, `ASTER_SRV_TOKEN` elsewhere,
//! and `ASTER_SRV_PROXY_TOKEN` as `Proxy-Authorization` behind IAP.
//!
//! A 403 on upload or commit means this identity cannot write (a pull-request
//! branch, or a person reading). The backend then answers writes as a no-op
//! for the rest of the process. Network errors are returned so [`TieredCache`]
//! can disable the remote after repeated failures. Tokens are never logged.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest as ShaDigest, Sha256};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use super::artifact::{Digest, Manifest};
use super::backend::CacheBackend;
use super::hasher::encode_hex;
use crate::test_results::upload::{proxy_token_from_env, Credentials};

/// A remote failure that should stop further attempts this run.
///
/// A timeout is one of these: retrying a hung gateway stalls the build.
#[derive(Debug)]
pub struct DisablingFailure(pub anyhow::Error);

impl std::fmt::Display for DisablingFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for DisablingFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

const USER_AGENT: &str = concat!("aster/", env!("CARGO_PKG_VERSION"));

/// Lookup accepts at most this many keys. Larger batches are split.
const MAX_KEYS: usize = 256;
/// Upload accepts at most this many digests.
const MAX_DIGESTS: usize = 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A hung gateway must not stall the build. Past this, the call is a failure
/// and the tier falls back to local. Three consecutive failures disable the
/// remote, so the worst stall is a few of these.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Signed blob transfers can be large, but still bounded.
const BLOB_TIMEOUT: Duration = Duration::from_secs(120);

/// One lookup hit: the decoded manifest and signed GET URLs for its blobs.
#[derive(Debug, Clone)]
struct Hit {
    manifest: Manifest,
    blobs: HashMap<String, String>,
}

/// Signed PUT URLs from one `upload` call, shared by the `put_blob` calls
/// that follow `find_missing`.
struct UploadGrant {
    missing: HashMap<String, String>,
}

struct State {
    /// Keys already asked of lookup. `None` is a confirmed miss.
    hits: HashMap<String, Option<Hit>>,
    /// The upload call whose URLs `put_blob` should consume.
    grant: Option<UploadGrant>,
    /// Set after a 403 on upload or commit. Writes become no-ops.
    read_only: bool,
}

/// [`CacheBackend`] for an aster-srv gateway.
pub struct AsterSrvBackend {
    base_url: String,
    repo: String,
    credentials: Credentials,
    proxy_token: Option<String>,
    agent: ureq::Agent,
    blob_agent: ureq::Agent,
    state: Mutex<State>,
    logged_read_only: AtomicBool,
}

impl AsterSrvBackend {
    /// Build a backend when `ASTER_SRV_URL` is set and credentials exist.
    ///
    /// `None` when the URL is empty, no credential is available, or `repo`
    /// is empty. Absence is not an error: the caller stays local-only.
    pub fn from_env(url: Option<&str>, repo: Option<&str>) -> Option<Self> {
        let env = |name: &str| std::env::var(name).ok();
        Self::from_parts(
            url,
            repo,
            Credentials::from_env(&env),
            proxy_token_from_env(&env),
        )
    }

    fn from_parts(
        url: Option<&str>,
        repo: Option<&str>,
        credentials: Option<Credentials>,
        proxy_token: Option<String>,
    ) -> Option<Self> {
        let base_url = non_empty(url)?;
        let repo = non_empty(repo)?;
        let credentials = credentials?;
        Some(Self::new(base_url, repo, credentials, proxy_token))
    }

    fn new(
        base_url: String,
        repo: String,
        credentials: Credentials,
        proxy_token: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            repo,
            credentials,
            proxy_token,
            agent: build_agent(REQUEST_TIMEOUT),
            blob_agent: build_agent(BLOB_TIMEOUT),
            state: Mutex::new(State {
                hits: HashMap::new(),
                grant: None,
                read_only: false,
            }),
            logged_read_only: AtomicBool::new(false),
        }
    }

    /// Ask lookup for every `keys` entry that is not already cached.
    ///
    /// The executor calls this once per ready batch, before any target in
    /// that batch looks up its manifest. A later [`CacheBackend::get_manifest`]
    /// answers from the cache.
    pub fn prefetch(&self, keys: &[String]) -> Result<()> {
        let pending: Vec<String> = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            keys.iter()
                .filter(|key| !key.is_empty() && !state.hits.contains_key(key.as_str()))
                .cloned()
                .collect()
        };
        if pending.is_empty() {
            return Ok(());
        }
        for chunk in pending.chunks(MAX_KEYS) {
            self.lookup_keys(chunk)?;
        }
        Ok(())
    }

    fn lookup_keys(&self, keys: &[String]) -> Result<()> {
        let body = json!({ "repo": self.repo, "keys": keys });
        let value = self.post("/v1/cache/lookup", &body, false)?;
        let hits = value
            .get("hits")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for key in keys {
            let hit = match hits.get(key) {
                Some(entry) => {
                    decode_hit(entry).with_context(|| format!("lookup hit for {key}"))?
                }
                None => None,
            };
            state.hits.insert(key.clone(), hit);
        }
        Ok(())
    }

    fn cached_hit(&self, key: &str) -> Result<Option<Hit>> {
        {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(hit) = state.hits.get(key) {
                return Ok(hit.clone());
            }
        }
        self.lookup_keys(std::slice::from_ref(&key.to_string()))?;
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Ok(state.hits.get(key).cloned().flatten())
    }

    fn post(&self, path: &str, body: &Value, write: bool) -> Result<Value> {
        let payload = serde_json::to_vec(body).context("encode cache request")?;
        let url = format!("{}{path}", self.base_url);
        let token = crate::test_results::upload::bearer(&self.agent, &self.credentials)
            .context("cannot authenticate to the remote cache")?;
        let mut request = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT);
        if let Some(proxy) = &self.proxy_token {
            request = request.header("Proxy-Authorization", &format!("Bearer {proxy}"));
        }
        let response = match request.send(&payload) {
            Ok(response) => response,
            Err(error) => {
                let error =
                    anyhow::Error::new(error).context(format!("cannot reach remote cache {path}"));
                let error = redact(error);
                if format!("{error:#}").contains("timeout") {
                    return Err(DisablingFailure(error).into());
                }
                return Err(error);
            }
        };
        let status = response.status().as_u16();
        if status == 403 && write {
            self.become_read_only();
            bail!("remote cache refused a write (403); continuing read-only");
        }
        let mut response = response;
        let text = response
            .body_mut()
            .read_to_string()
            .context("read remote cache response")?;
        if !(200..300).contains(&status) {
            bail!(
                "remote cache {path} returned {status}: {}",
                brief(&redact_text(&text))
            );
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("parse remote cache response")
    }

    fn become_read_only(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.read_only = true;
            state.grant = None;
        }
        if self
            .logged_read_only
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            eprintln!(
                "[aster] Warning: remote cache is read-only for this run; uploads are skipped"
            );
        }
    }

    fn read_only(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.read_only)
            .unwrap_or(true)
    }

    /// Stream `url` into `writer`, returning the sha256 of the bytes written.
    fn stream_get(&self, url: &str, writer: &mut dyn Write) -> Result<String> {
        let response = self
            .blob_agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|error| redact(anyhow::Error::new(error)))
            .context("cannot download remote blob")?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("remote blob download returned {status}");
        }
        let mut reader = response.into_body().into_reader();
        let mut hasher = Sha256::new();
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).context("read remote blob")?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).context("write remote blob")?;
            ShaDigest::update(&mut hasher, &buf[..n]);
        }
        Ok(encode_hex(ShaDigest::finalize(hasher).as_ref()))
    }

    fn stream_put(&self, url: &str, reader: &mut dyn Read) -> Result<()> {
        // ureq sends a file or a buffer, not a trait object. The spool is
        // unnamed and removed when this call returns.
        let mut spool = tempfile::NamedTempFile::new().context("create upload spool")?;
        io::copy(reader, spool.as_file_mut()).context("buffer remote blob upload")?;
        spool
            .as_file_mut()
            .sync_all()
            .context("flush upload spool")?;
        let file = spool.reopen().context("reopen upload spool")?;
        let response = self
            .blob_agent
            .put(url)
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/octet-stream")
            .send(file)
            .map_err(|error| redact(anyhow::Error::new(error)))
            .context("cannot upload remote blob")?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("remote blob upload returned {status}");
        }
        Ok(())
    }

    /// Whether a lookup hit named a signed GET for `digest`.
    ///
    /// The server omits a blob that is not in the bucket, so a missing URL is
    /// a miss. This does not call `/upload`, which is writers-only.
    pub fn has_blob(&self, digest: &Digest) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .hits
            .values()
            .flatten()
            .any(|hit| {
                hit.blobs
                    .get(digest.as_str())
                    .is_some_and(|url| !url.is_empty())
            })
    }
}

impl CacheBackend for AsterSrvBackend {
    fn get_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        Ok(self.cached_hit(key)?.map(|hit| hit.manifest))
    }

    fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
        if self.read_only() {
            return Ok(());
        }
        let body = json!({
            "repo": self.repo,
            "key": manifest.key,
            "manifest": BASE64.encode(manifest.encode()),
        });
        match self.post("/v1/cache/commit", &body, true) {
            Ok(_) => Ok(()),
            Err(error) if self.read_only() => {
                // 403 already switched the backend; the local store stands.
                let _ = error;
                Ok(())
            }
            Err(error) if is_conflict(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>> {
        if digests.is_empty() || self.read_only() {
            return Ok(Vec::new());
        }
        let names: Vec<String> = digests.iter().map(|d| d.as_str().to_string()).collect();
        let mut missing = Vec::new();
        let mut urls = HashMap::new();
        for chunk in names.chunks(MAX_DIGESTS) {
            let body = json!({ "repo": self.repo, "digests": chunk });
            let value = match self.post("/v1/cache/upload", &body, true) {
                Ok(value) => value,
                Err(_error) if self.read_only() => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            let listed = value
                .get("missing")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for name in chunk {
                if let Some(url) = listed.get(name).and_then(Value::as_str) {
                    missing.push(Digest::from_hex(name.clone()));
                    urls.insert(name.clone(), url.to_string());
                }
            }
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.grant = Some(UploadGrant { missing: urls });
        Ok(missing)
    }

    fn get_blob(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool> {
        let url = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.hits.values().flatten().find_map(|hit| {
                hit.blobs
                    .get(digest.as_str())
                    .filter(|url| !url.is_empty())
                    .cloned()
            })
        };
        let Some(url) = url else {
            return Ok(false);
        };
        let hashed = self.stream_get(&url, writer)?;
        if hashed != digest.as_str() {
            bail!(
                "remote blob digest mismatch: expected {}, got {hashed}",
                digest.as_str()
            );
        }
        Ok(true)
    }

    fn put_blob(&self, digest: &Digest, reader: &mut dyn Read) -> Result<()> {
        if self.read_only() {
            return Ok(());
        }
        let url = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state
                .grant
                .as_ref()
                .and_then(|grant| grant.missing.get(digest.as_str()).cloned())
        };
        let Some(url) = url else {
            bail!(
                "no upload URL for {}; find_missing must run first",
                digest.as_str()
            );
        };
        self.stream_put(&url, reader)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Downcast is unavailable across crate errors, so 409 is matched on the
/// message `post` builds. The status is the only token in that message.
fn is_conflict(error: &anyhow::Error) -> bool {
    error.to_string().contains("returned 409")
}

fn decode_hit(entry: &Value) -> Result<Option<Hit>> {
    let encoded = entry
        .get("manifest")
        .and_then(Value::as_str)
        .context("lookup hit has no manifest")?;
    let bytes = BASE64
        .decode(encoded.trim())
        .context("lookup manifest is not base64")?;
    // An undecodable manifest is a miss, same as a local decode failure.
    let Some(manifest) = Manifest::decode(&bytes) else {
        return Ok(None);
    };
    let mut blobs = HashMap::new();
    if let Some(object) = entry.get("blobs").and_then(Value::as_object) {
        for (digest, url) in object {
            if let Some(url) = url.as_str() {
                blobs.insert(digest.clone(), url.to_string());
            }
        }
    }
    Ok(Some(Hit { manifest, blobs }))
}

fn non_empty(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Drop query strings. Signed URLs carry credentials there, and ureq names
/// the URL in a transport error.
fn redact(error: anyhow::Error) -> anyhow::Error {
    let text = redact_text(&format!("{error:#}"));
    anyhow::anyhow!("{text}")
}

fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('?') {
        let end = rest[start..]
            .find(|ch: char| ch.is_whitespace() || ch == '"' || ch == '>')
            .map(|index| start + index)
            .unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        out.push_str("?redacted");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A short, token-free summary of an error body. Headers are never included.
fn brief(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty body".to_string();
    }
    let message = serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| trimmed.chars().take(200).collect());
    message.replace('\n', " ")
}

fn build_agent(timeout: Duration) -> ureq::Agent {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .root_certs(RootCerts::WebPki)
        .build();
    ureq::Agent::config_builder()
        .https_only(false)
        .http_status_as_error(false)
        .tls_config(tls)
        .user_agent(USER_AGENT)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(timeout))
        .build()
        .into()
}

/// `owner/name` for the cache, matching test-result uploads.
pub fn repo_from_env(workspace_root: &std::path::Path) -> String {
    crate::test_results::detect_run(workspace_root, "", None, &|name| std::env::var(name).ok()).repo
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Cursor};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn manifest(key: &str, digest: &Digest) -> Manifest {
        Manifest {
            schema: super::super::artifact::MANIFEST_SCHEMA_VERSION,
            key: key.to_string(),
            target: "//app:build".to_string(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-17T00:00:00Z".to_string(),
            outputs: vec![super::super::artifact::OutputEntry {
                path: "out/file.txt".into(),
                kind: super::super::artifact::OutputKind::File,
                digest: Some(digest.clone()),
                mode: 0o644,
                target: None,
            }],
            log: None,
            aster_version: "0.21.0".to_string(),
        }
    }

    struct Fake {
        url: String,
        lookups: Arc<AtomicUsize>,
        #[allow(dead_code)]
        uploads: Arc<AtomicUsize>,
        commits: Arc<AtomicUsize>,
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        commit_status: Arc<Mutex<u16>>,
        shutdown: Arc<AtomicBool>,
    }

    impl Fake {
        fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let lookups = Arc::new(AtomicUsize::new(0));
            let uploads = Arc::new(AtomicUsize::new(0));
            let commits = Arc::new(AtomicUsize::new(0));
            let objects = Arc::new(Mutex::new(HashMap::new()));
            let manifests: Arc<Mutex<HashMap<String, Vec<u8>>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let commit_status = Arc::new(Mutex::new(201_u16));
            let shutdown = Arc::new(AtomicBool::new(false));
            let base = url.clone();
            let lookups_c = Arc::clone(&lookups);
            let uploads_c = Arc::clone(&uploads);
            let commits_c = Arc::clone(&commits);
            let objects_c = Arc::clone(&objects);
            let manifests_c = Arc::clone(&manifests);
            let status_c = Arc::clone(&commit_status);
            let shutdown_c = Arc::clone(&shutdown);
            thread::spawn(move || {
                listener.set_nonblocking(false).ok();
                for stream in listener.incoming() {
                    if shutdown_c.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut reader = BufReader::new(&stream);
                    let mut request = String::new();
                    if reader.read_line(&mut request).is_err() || request.is_empty() {
                        continue;
                    }
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty()
                        {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(value) = lower.strip_prefix("content-length:") {
                            content_length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0_u8; content_length];
                    if content_length > 0 {
                        use std::io::Read as _;
                        if reader.read_exact(&mut body).is_err() {
                            continue;
                        }
                    }
                    let path = request.split_whitespace().nth(1).unwrap_or("");
                    let method = request.split_whitespace().next().unwrap_or("");
                    if path.starts_with("/objects/") {
                        let digest = path.trim_start_matches("/objects/");
                        let payload = if method == "PUT" {
                            objects_c.lock().unwrap().insert(digest.to_string(), body);
                            Vec::new()
                        } else {
                            objects_c
                                .lock()
                                .unwrap()
                                .get(digest)
                                .cloned()
                                .unwrap_or_default()
                        };
                        let status = if method == "GET"
                            && payload.is_empty()
                            && !objects_c.lock().unwrap().contains_key(digest)
                        {
                            404
                        } else {
                            200
                        };
                        let response = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(&payload);
                        continue;
                    }
                    let (status, payload) = route(
                        path,
                        &body,
                        &base,
                        &lookups_c,
                        &uploads_c,
                        &commits_c,
                        &objects_c,
                        &manifests_c,
                        &status_c,
                    );
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(&payload);
                }
            });
            Self {
                url,
                lookups,
                uploads,
                commits,
                objects,
                commit_status,
                shutdown,
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn route(
        path: &str,
        body: &[u8],
        base: &str,
        lookups: &AtomicUsize,
        uploads: &AtomicUsize,
        commits: &AtomicUsize,
        objects: &Mutex<HashMap<String, Vec<u8>>>,
        manifests: &Mutex<HashMap<String, Vec<u8>>>,
        commit_status: &Mutex<u16>,
    ) -> (u16, Vec<u8>) {
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
                    if let Some(manifest) = Manifest::decode(bytes) {
                        for digest in manifest.blob_refs() {
                            if blobs.contains_key(digest.as_str()) {
                                urls.insert(
                                    digest.as_str().to_string(),
                                    Value::String(format!("{base}/objects/{}", digest.as_str())),
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
                (200, serde_json::to_vec(&json!({"hits": hits})).unwrap())
            }
            "/v1/cache/upload" => {
                uploads.fetch_add(1, Ordering::Relaxed);
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
                )
            }
            "/v1/cache/commit" => {
                commits.fetch_add(1, Ordering::Relaxed);
                let status = *commit_status.lock().unwrap();
                if status != 201 {
                    return (
                        status,
                        serde_json::to_vec(&json!({"error": "refused"})).unwrap(),
                    );
                }
                let key = request["key"].as_str().unwrap_or("");
                let encoded = request["manifest"].as_str().unwrap_or("");
                let bytes = BASE64.decode(encoded).unwrap_or_default();
                manifests.lock().unwrap().insert(key.to_string(), bytes);
                (201, br#"{"stored":true}"#.to_vec())
            }
            _ => (404, br#"{"error":"not found"}"#.to_vec()),
        }
    }

    fn backend(fake: &Fake) -> AsterSrvBackend {
        AsterSrvBackend::from_parts(
            Some(&fake.url),
            Some("acme/app"),
            Some(Credentials::Static("test-token".to_string())),
            None,
        )
        .unwrap()
    }

    #[test]
    fn prefetch_answers_get_manifest_without_another_lookup() {
        let fake = Fake::spawn();
        let digest = Digest::of_bytes(b"blob");
        let stored = manifest(&"ab".repeat(32), &digest);
        let client = backend(&fake);
        let missing = client.find_missing(std::slice::from_ref(&digest)).unwrap();
        assert_eq!(missing, vec![digest.clone()]);
        client
            .put_blob(&digest, &mut Cursor::new(b"blob".to_vec()))
            .unwrap();
        client.put_manifest(&stored).unwrap();

        let reader = backend(&fake);
        reader.prefetch(std::slice::from_ref(&stored.key)).unwrap();
        assert_eq!(fake.lookups.load(Ordering::Relaxed), 1);
        let hit = reader.get_manifest(&stored.key).unwrap().unwrap();
        assert_eq!(hit, stored);
        assert_eq!(fake.lookups.load(Ordering::Relaxed), 1);
        assert!(reader.get_manifest("missing").unwrap().is_none());
    }

    #[test]
    fn get_blob_rejects_a_digest_mismatch() {
        let fake = Fake::spawn();
        let digest = Digest::of_bytes(b"expected");
        let stored = manifest(&"cd".repeat(32), &digest);
        let client = backend(&fake);
        client.put_manifest(&stored).unwrap();
        fake.objects
            .lock()
            .unwrap()
            .insert(digest.as_str().to_string(), b"tampered".to_vec());

        let reader = backend(&fake);
        reader.get_manifest(&stored.key).unwrap().unwrap();
        let mut out = Vec::new();
        let error = reader.get_blob(&digest, &mut out).unwrap_err();
        assert!(error.to_string().contains("digest mismatch"), "{error:#}");
    }

    #[test]
    fn forbidden_commit_switches_to_read_only() {
        let fake = Fake::spawn();
        *fake.commit_status.lock().unwrap() = 403;
        let digest = Digest::of_bytes(b"x");
        let stored = manifest(&"ef".repeat(32), &digest);
        let client = backend(&fake);
        client.put_manifest(&stored).unwrap();
        assert_eq!(fake.commits.load(Ordering::Relaxed), 1);
        // A second write does not call the server.
        client.put_manifest(&stored).unwrap();
        assert_eq!(fake.commits.load(Ordering::Relaxed), 1);
        assert!(client.read_only());
    }

    #[test]
    fn from_env_is_none_without_a_url_or_token() {
        assert!(AsterSrvBackend::from_parts(None, Some("acme/app"), None, None).is_none());
        assert!(AsterSrvBackend::from_parts(
            Some("http://127.0.0.1:9"),
            Some("acme/app"),
            None,
            None
        )
        .is_none());
        assert!(AsterSrvBackend::from_parts(
            Some("  "),
            Some("acme/app"),
            Some(Credentials::Static("t".into())),
            None
        )
        .is_none());
    }

    #[test]
    fn redact_strips_signed_query_strings() {
        let error = redact(anyhow::anyhow!(
            "GET https://storage.example/cas/abc?X-Goog-Signature=secret&token=s3cr3t failed"
        ));
        let text = format!("{error:#}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("s3cr3t"), "{text}");
        assert!(text.contains("?redacted"), "{text}");
    }
}
