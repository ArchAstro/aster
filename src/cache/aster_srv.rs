//! aster-srv cache client.
//!
//! Speaks `POST /v1/cache/lookup`, `/upload`, and `/commit`. Blob bytes move
//! through the signed URLs those return. CI credentials are the ones
//! test-result uploads already use: GitHub OIDC in Actions, `ASTER_SRV_TOKEN`
//! elsewhere, and `ASTER_SRV_PROXY_TOKEN` as `Proxy-Authorization` (still sent
//! for deployments that keep an identity-aware proxy in front). With no CI
//! credential, a person uses the token `aster srv login` stored; with
//! neither, the remote is skipped with one line and the build stays local.
//! A person's token reads only, so writes are never attempted with it.
//!
//! A 403 on upload or commit means this identity cannot write (a pull-request
//! branch). The backend then answers writes as a no-op for the rest of the
//! process. Network errors are returned so [`TieredCache`] can disable the
//! remote after repeated failures. Tokens are never logged.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest as ShaDigest, Sha256};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use super::artifact::{Digest, Manifest};
use super::backend::CacheBackend;
use super::hasher::encode_hex;
use crate::srv_url::{checked_origin, scheme_policy};
use crate::test_results::upload::{proxy_token_from_env, Credentials};

use super::login;

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

struct State {
    /// Keys already asked of lookup. `None` is a confirmed miss.
    hits: HashMap<String, Option<Hit>>,
    /// Signed PUT URLs from `upload` calls, keyed by digest. Merged, not
    /// replaced, so concurrent stores keep each other's URLs.
    upload_urls: HashMap<String, String>,
    /// Set after a 403 on upload or commit. Writes become no-ops.
    read_only: bool,
}

/// [`CacheBackend`] for an aster-srv gateway.
pub struct AsterSrvBackend {
    base_url: String,
    repo: String,
    /// CI credential. Absent for a person reading with a stored sign-in.
    credentials: Option<Credentials>,
    /// Sent as `Proxy-Authorization` when set, whatever else is used.
    proxy_token: Option<String>,
    /// The token `aster srv login` stored, used when there is no CI
    /// credential. Reads only.
    person_token: Option<String>,
    /// When set, blob URLs on `storage.googleapis.com` are fetched here
    /// instead. Tests only; production is `None`.
    gcs_base: Option<String>,
    agent: ureq::Agent,
    blob_agent: ureq::Agent,
    state: Mutex<State>,
    logged_read_only: AtomicBool,
}

impl AsterSrvBackend {
    /// Build a backend when `ASTER_SRV_URL` is set.
    ///
    /// CI uses `ASTER_SRV_TOKEN` or GitHub OIDC. A laptop has neither: it
    /// uses the token `aster srv login` stored for this server, and with
    /// no unexpired token the remote is skipped. `None` when the URL or repo
    /// is empty.
    ///
    /// A URL that is plain http to another machine is an error: the
    /// credentials would cross the network in the clear.
    pub fn from_env(url: Option<&str>, repo: Option<&str>) -> Result<Option<Self>> {
        let env = |name: &str| std::env::var(name).ok();
        let credentials = Credentials::from_env(&env);
        let person_token = match (&credentials, url) {
            (None, Some(url)) => {
                login::stored_token(url, SystemTime::now()).map(|stored| stored.token)
            }
            _ => None,
        };
        let Some(backend) = Self::from_parts(
            url,
            repo,
            credentials,
            proxy_token_from_env(&env),
            person_token,
        )?
        else {
            return Ok(None);
        };
        if let Some(notice) = signature_exclusion_notice(super::signature::signing_key().is_some())
        {
            eprintln!("{notice}");
        }
        Ok(Some(backend))
    }

    fn from_parts(
        url: Option<&str>,
        repo: Option<&str>,
        credentials: Option<Credentials>,
        proxy_token: Option<String>,
        person_token: Option<String>,
    ) -> Result<Option<Self>> {
        let (Some(base_url), Some(repo)) = (non_empty(url), non_empty(repo)) else {
            return Ok(None);
        };
        Self::new(base_url, repo, credentials, proxy_token, person_token).map(Some)
    }

    /// Rewrite `https://storage.googleapis.com` blob URLs to `base`.
    ///
    /// Production leaves this unset and talks to GCS. The end-to-end test
    /// sets it so a local stand-in can hold the bytes a signed URL names,
    /// without changing the URL the server signs.
    pub fn redirect_gcs_to(mut self, base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        if base.is_empty() {
            return self;
        }
        match checked_origin(&base) {
            Ok(origin) => {
                self.blob_agent = build_agent(&origin, BLOB_TIMEOUT);
                self.gcs_base = Some(base);
            }
            Err(error) => eprintln!("[aster] Warning: ignoring ASTER_SRV_GCS_BASE: {error:#}"),
        }
        self
    }

    /// Replace the per-call timeout. Tests shrink it so a slow fake gateway
    /// reproduces a production timeout without a multi-second wait.
    #[cfg(test)]
    fn with_request_timeout(mut self, timeout: Duration) -> Self {
        let origin = checked_origin(&self.base_url).expect("checked in new");
        self.agent = build_agent(&origin, timeout);
        self
    }

    fn new(
        base_url: String,
        repo: String,
        credentials: Option<Credentials>,
        proxy_token: Option<String>,
        person_token: Option<String>,
    ) -> Result<Self> {
        let origin = checked_origin(&base_url)?;
        // A person's token reads only; do not ask for upload URLs it cannot get.
        let person_only = credentials.is_none() && person_token.is_some();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            repo,
            credentials,
            proxy_token,
            person_token,
            gcs_base: None,
            agent: build_agent(&origin, REQUEST_TIMEOUT),
            // Signed blob URLs come from the server's reply, so they get the
            // server's policy: https only, unless the server is this machine.
            blob_agent: build_agent(&origin, BLOB_TIMEOUT),
            state: Mutex::new(State {
                hits: HashMap::new(),
                upload_urls: HashMap::new(),
                read_only: person_only,
            }),
            logged_read_only: AtomicBool::new(false),
        })
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
        let bearer = match (&self.credentials, &self.person_token) {
            (Some(credentials), _) => crate::test_results::upload::bearer(&self.agent, credentials)
                .context("cannot authenticate to the remote cache")?,
            (None, Some(token)) => token.clone(),
            (None, None) => return Err(signed_out()),
        };
        let response =
            self.send_post(&url, &payload, Some(&bearer), self.proxy_token.as_deref())?;
        // A stored sign-in the server no longer accepts (expired early, key
        // rotated, access removed) is the same as none.
        if response.status().as_u16() == 401 && self.credentials.is_none() {
            return Err(signed_out());
        }
        self.decode_post(path, response, write)
    }

    fn send_post(
        &self,
        url: &str,
        payload: &[u8],
        bearer: Option<&str>,
        proxy: Option<&str>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let mut request = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT);
        if let Some(token) = bearer {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        if let Some(proxy) = proxy {
            request = request.header("Proxy-Authorization", &format!("Bearer {proxy}"));
        }
        request.send(payload).map_err(|error| {
            let error = anyhow::Error::new(error);
            let error = redact(error);
            if format!("{error:#}").contains("timeout") {
                DisablingFailure(error).into()
            } else {
                error
            }
        })
    }

    fn decode_post(
        &self,
        path: &str,
        response: ureq::http::Response<ureq::Body>,
        write: bool,
    ) -> Result<Value> {
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
            state.upload_urls.clear();
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
        let url = self.blob_url(url);
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
        let url = self.blob_url(url);
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

    /// The URL blob bytes actually move through.
    ///
    /// A test stand-in replaces the GCS host and keeps the signed path, so
    /// the object name the server signed is the object the stand-in stores.
    fn blob_url(&self, url: &str) -> String {
        let Some(base) = &self.gcs_base else {
            return url.to_string();
        };
        let Some(rest) = url.strip_prefix("https://storage.googleapis.com") else {
            return url.to_string();
        };
        format!("{base}{rest}")
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
        let encoded = manifest.encode();
        let body = json!({
            "repo": self.repo,
            "key": manifest.key,
            "manifest": BASE64.encode(&encoded),
        });
        // Do not attach a signature. aster-srv drops unknown commit fields
        // and lookup does not return one, so a required sig would miss every hit.
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
        state.upload_urls.extend(urls);
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
            state.upload_urls.get(digest.as_str()).cloned()
        };
        let Some(url) = url else {
            bail!(
                "no upload URL for {}; find_missing must run first",
                digest.as_str()
            );
        };
        self.stream_put(&url, reader)
    }

    fn writable(&self) -> bool {
        !self.read_only()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// aster-srv is excluded from manifest signing: the server drops a `sig`
/// on commit and never returns one, so lookups trust its authenticated
/// API instead. When a key is set, say so once rather than skip the check
/// silently.
fn signature_exclusion_notice(key_set: bool) -> Option<&'static str> {
    key_set.then_some(
        "[aster] Note: ASTER_REMOTE_CACHE_SIGNATURE_KEY does not apply to the aster-srv cache; \
         its manifests are not signature-verified and are trusted from the authenticated server",
    )
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
    // aster-srv does not store or return a signature (see put_manifest).
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

/// The remote is skipped: say so once per process, since the executor builds
/// a backend per target, and stop asking.
fn signed_out() -> anyhow::Error {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| eprintln!("{}", login::SIGNED_OUT_MESSAGE));
    DisablingFailure(anyhow::anyhow!(SIGNED_OUT_REASON)).into()
}

const SIGNED_OUT_REASON: &str = "remote cache skipped; not signed in";

/// Whether `error` is [`signed_out`], whose own warning is already printed.
pub fn is_signed_out(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string() == SIGNED_OUT_REASON)
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

/// An agent for `origin` (already through `checked_origin`): https only
/// unless `origin` is this machine, and no redirect can downgrade.
pub(crate) fn build_agent(origin: &str, timeout: Duration) -> ureq::Agent {
    let (https_only, max_redirects) = scheme_policy(origin);
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .root_certs(RootCerts::WebPki)
        .build();
    ureq::Agent::config_builder()
        .https_only(https_only)
        .max_redirects(max_redirects)
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
            packs: Vec::new(),
        }
    }

    struct Fake {
        url: String,
        lookups: Arc<AtomicUsize>,
        #[allow(dead_code)]
        uploads: Arc<AtomicUsize>,
        commits: Arc<AtomicUsize>,
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        manifests: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        commit_status: Arc<Mutex<u16>>,
        /// When set, every cache call is refused with 401.
        refuse: Arc<AtomicBool>,
        /// Milliseconds a lookup waits per key it was asked about. The real
        /// server signs and checks every blob of every hit one at a time, so
        /// its lookup time grows with the batch.
        lookup_ms_per_key: Arc<std::sync::atomic::AtomicU64>,
        proxy_seen: Arc<Mutex<Vec<String>>>,
        /// The `Authorization` header of each cache call (lowercased).
        auth_seen: Arc<Mutex<Vec<String>>>,
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
            let refuse = Arc::new(AtomicBool::new(false));
            let lookup_ms_per_key = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let proxy_seen = Arc::new(Mutex::new(Vec::new()));
            let auth_seen = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let base = url.clone();
            let lookups_c = Arc::clone(&lookups);
            let uploads_c = Arc::clone(&uploads);
            let commits_c = Arc::clone(&commits);
            let objects_c = Arc::clone(&objects);
            let manifests_c = Arc::clone(&manifests);
            let status_c = Arc::clone(&commit_status);
            let refuse_c = Arc::clone(&refuse);
            let delay_c = Arc::clone(&lookup_ms_per_key);
            let proxy_seen_c = Arc::clone(&proxy_seen);
            let auth_seen_c = Arc::clone(&auth_seen);
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
                    let mut proxy_auth = String::new();
                    let mut auth = String::new();
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
                        if let Some(value) = lower.strip_prefix("proxy-authorization:") {
                            proxy_auth = value.trim().to_string();
                        }
                        if let Some(value) = lower.strip_prefix("authorization:") {
                            auth = value.trim().to_string();
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
                    proxy_seen_c.lock().unwrap().push(proxy_auth.clone());
                    auth_seen_c.lock().unwrap().push(auth);
                    if refuse_c.load(Ordering::Relaxed) {
                        let payload = br#"{"error":"the token is not valid"}"#;
                        let response = format!(
                            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(payload);
                        continue;
                    }
                    if path == "/v1/cache/lookup" {
                        let asked = serde_json::from_slice::<Value>(&body)
                            .ok()
                            .and_then(|request| request["keys"].as_array().map(Vec::len))
                            .unwrap_or(0) as u64;
                        thread::sleep(Duration::from_millis(
                            asked * delay_c.load(Ordering::Relaxed),
                        ));
                    }
                    let (status, payload, extra_headers) = route(
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
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
                manifests,
                commit_status,
                refuse,
                lookup_ms_per_key,
                proxy_seen,
                auth_seen,
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
    ) -> (u16, Vec<u8>, String) {
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
                (
                    200,
                    serde_json::to_vec(&json!({"hits": hits})).unwrap(),
                    String::new(),
                )
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
                    String::new(),
                )
            }
            "/v1/cache/commit" => {
                commits.fetch_add(1, Ordering::Relaxed);
                let status = *commit_status.lock().unwrap();
                if status != 201 {
                    return (
                        status,
                        serde_json::to_vec(&json!({"error": "refused"})).unwrap(),
                        String::new(),
                    );
                }
                let key = request["key"].as_str().unwrap_or("");
                let encoded = request["manifest"].as_str().unwrap_or("");
                let bytes = BASE64.decode(encoded).unwrap_or_default();
                manifests.lock().unwrap().insert(key.to_string(), bytes);
                (201, br#"{"stored":true}"#.to_vec(), String::new())
            }
            _ => (404, br#"{"error":"not found"}"#.to_vec(), String::new()),
        }
    }

    fn backend(fake: &Fake) -> AsterSrvBackend {
        AsterSrvBackend::from_parts(
            Some(&fake.url),
            Some("acme/app"),
            Some(Credentials::Static("test-token".to_string())),
            None,
            None,
        )
        .unwrap()
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
    fn concurrent_find_missing_keeps_each_stores_upload_url() {
        let fake = Fake::spawn();
        let first = Digest::of_bytes(b"first");
        let second = Digest::of_bytes(b"second");
        let client = backend(&fake);
        // Two stores reserve URLs before either uploads.
        assert_eq!(
            client.find_missing(std::slice::from_ref(&first)).unwrap(),
            vec![first.clone()]
        );
        assert_eq!(
            client.find_missing(std::slice::from_ref(&second)).unwrap(),
            vec![second.clone()]
        );
        client
            .put_blob(&first, &mut Cursor::new(b"first".to_vec()))
            .unwrap();
        client
            .put_blob(&second, &mut Cursor::new(b"second".to_vec()))
            .unwrap();
        let objects = fake.objects.lock().unwrap();
        assert_eq!(objects.get(first.as_str()).unwrap(), b"first");
        assert_eq!(objects.get(second.as_str()).unwrap(), b"second");
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
    fn from_env_is_none_without_a_url_or_repo() {
        assert!(
            AsterSrvBackend::from_parts(None, Some("acme/app"), None, None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            AsterSrvBackend::from_parts(
                Some("http://127.0.0.1:9"),
                Some("acme/app"),
                None,
                None,
                None
            )
            .unwrap()
            .is_some(),
            "a person with no CI token still gets a backend, which stays local until signed in"
        );
        assert!(AsterSrvBackend::from_parts(
            Some("  "),
            Some("acme/app"),
            Some(Credentials::Static("t".into())),
            None,
            None,
        )
        .unwrap()
        .is_none());
        assert!(
            AsterSrvBackend::from_parts(Some("http://x"), Some(" "), None, None, None)
                .unwrap()
                .is_none()
        );
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

    fn reader(fake: &Fake, person_token: Option<&str>) -> AsterSrvBackend {
        AsterSrvBackend::from_parts(
            Some(&fake.url),
            Some("acme/app"),
            None,
            None,
            person_token.map(str::to_string),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn a_signed_in_person_reads_with_their_token_and_never_asks_to_write() {
        let fake = Fake::spawn();
        let digest = Digest::of_bytes(b"blob");
        let stored = manifest(&"ab".repeat(32), &digest);
        // CI stores the artifact the laptop will read.
        let writer = backend(&fake);
        writer.find_missing(std::slice::from_ref(&digest)).unwrap();
        writer
            .put_blob(&digest, &mut Cursor::new(b"blob".to_vec()))
            .unwrap();
        writer.put_manifest(&stored).unwrap();
        let before = fake.uploads.load(Ordering::Relaxed);
        let commits = fake.commits.load(Ordering::Relaxed);

        let person = reader(&fake, Some("person-token"));
        let hit = person.get_manifest(&stored.key).unwrap().unwrap();
        assert_eq!(hit, stored);
        let seen = fake.auth_seen.lock().unwrap().clone();
        assert_eq!(seen.last().map(String::as_str), Some("bearer person-token"));

        // The token reads only: no upload or commit request is sent.
        assert!(person
            .find_missing(std::slice::from_ref(&digest))
            .unwrap()
            .is_empty());
        person.put_manifest(&stored).unwrap();
        assert_eq!(fake.uploads.load(Ordering::Relaxed), before);
        assert_eq!(fake.commits.load(Ordering::Relaxed), commits);
    }

    #[test]
    fn a_ci_credential_wins_over_a_stored_person_token_and_the_proxy_header_still_goes() {
        let fake = Fake::spawn();
        let ci = AsterSrvBackend::from_parts(
            Some(&fake.url),
            Some("acme/app"),
            Some(Credentials::Static("ci-token".into())),
            Some("ci-proxy".into()),
            Some("person-token".into()),
        )
        .unwrap()
        .unwrap();
        ci.get_manifest("anything").unwrap();
        assert_eq!(
            fake.auth_seen.lock().unwrap().last().map(String::as_str),
            Some("bearer ci-token")
        );
        assert_eq!(
            fake.proxy_seen.lock().unwrap().last().map(String::as_str),
            Some("bearer ci-proxy")
        );
        // A CI backend is not read-only up front.
        assert!(!ci.read_only());
    }

    #[test]
    fn without_a_sign_in_the_remote_is_skipped_and_nothing_is_sent() {
        let fake = Fake::spawn();
        let signed_out = reader(&fake, None);
        let error = signed_out.get_manifest("anything").unwrap_err();
        assert!(disables(&error), "{error:#}");
        assert_eq!(fake.lookups.load(Ordering::Relaxed), 0);
        assert!(fake.auth_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_token_the_server_refuses_is_the_same_as_none() {
        let fake = Fake::spawn();
        fake.refuse.store(true, Ordering::Relaxed);
        let person = reader(&fake, Some("stale-token"));
        let error = person.get_manifest("anything").unwrap_err();
        assert!(disables(&error), "{error:#}");
        // A refused CI credential is an ordinary failure, not a sign-out.
        let ci = backend(&fake);
        let error = ci.get_manifest("anything").unwrap_err();
        assert!(!disables(&error), "{error:#}");
    }

    fn tier_over(
        fake: &Fake,
        timeout: Duration,
        dir: &std::path::Path,
    ) -> (
        super::super::backend::TieredCache,
        Arc<super::super::remote_stats::RemoteStats>,
    ) {
        let stats = Arc::new(super::super::remote_stats::RemoteStats::new());
        let tier = super::super::backend::TieredCache::with_remote(
            super::super::local::LocalStore::new(dir.to_path_buf()),
            super::super::backend::CacheMode::ReadWrite,
            Arc::new(backend(fake).with_request_timeout(timeout)),
        )
        .with_stats(Arc::clone(&stats));
        (tier, stats)
    }

    fn publish(fake: &Fake, count: u8) -> Vec<String> {
        let writer = backend(fake);
        (0..count)
            .map(|index| {
                let bytes = format!("blob-{index}").into_bytes();
                let digest = Digest::of_bytes(&bytes);
                let key = format!("{index:02x}").repeat(32);
                writer.find_missing(std::slice::from_ref(&digest)).unwrap();
                writer.put_blob(&digest, &mut Cursor::new(bytes)).unwrap();
                writer.put_manifest(&manifest(&key, &digest)).unwrap();
                key
            })
            .collect()
    }

    /// P3. The reader's first batch lookup took longer than the request
    /// timeout. Before, that disabled the remote for the whole run without a
    /// word, so every target rebuilt. Now the batch is only a missed
    /// optimisation: targets look themselves up one at a time and hit.
    #[test]
    fn a_batch_lookup_timeout_does_not_switch_the_remote_off_for_the_run() {
        let fake = Fake::spawn();
        let keys = publish(&fake, 3);
        // 3 keys take 1.5s, past the 1.2s timeout; one key takes 0.5s.
        fake.lookup_ms_per_key.store(500, Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        let (tier, stats) = tier_over(&fake, Duration::from_millis(1200), dir.path());

        tier.prefetch(&keys);
        assert!(tier.remote_enabled(), "a timed-out batch must not disable");
        for key in &keys {
            assert!(tier.lookup(key).unwrap().is_some(), "{key} must still hit");
        }

        let summary = stats.snapshot().unwrap();
        assert_eq!((summary.hits, summary.misses, summary.failures), (3, 0, 1));
        assert_eq!(summary.disabled, None, "{summary}");
    }

    /// A lookup of one target that times out still stops the run's remote
    /// calls (a hung gateway is one wait), and now the summary says why.
    #[test]
    fn a_single_lookup_timeout_disables_the_remote_and_the_summary_names_it() {
        let fake = Fake::spawn();
        let keys = publish(&fake, 1);
        fake.lookup_ms_per_key.store(1500, Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        let (tier, stats) = tier_over(&fake, Duration::from_millis(300), dir.path());

        assert!(tier.lookup(&keys[0]).unwrap().is_none());
        assert!(!tier.remote_enabled());
        let line = stats.snapshot().unwrap().to_string();
        assert!(
            line.starts_with("remote: 0 hits / 0 misses / 0 uploads / 1 failures; disabled: "),
            "{line}"
        );
        assert!(line.contains("timeout"), "{line}");
    }

    #[test]
    fn a_read_only_identity_reports_read_only_and_no_uploads() {
        let fake = Fake::spawn();
        *fake.commit_status.lock().unwrap() = 403;
        let dir = tempfile::tempdir().unwrap();
        let (tier, stats) = tier_over(&fake, Duration::from_secs(5), dir.path());
        let digest = Digest::of_bytes(b"pr-branch");
        tier.local_store()
            .put_blob(&digest, &b"pr-branch"[..])
            .unwrap();
        tier.store(&manifest(&"ee".repeat(32), &digest)).unwrap();
        assert_eq!(
            stats.snapshot().unwrap().to_string(),
            "remote: 0 hits / 0 misses / 0 uploads (read-only)"
        );
    }

    fn disables(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<DisablingFailure>().is_some())
    }

    #[test]
    fn signature_key_does_not_change_the_aster_srv_commit() {
        let _guard = env_lock();
        let previous = std::env::var_os("ASTER_REMOTE_CACHE_SIGNATURE_KEY");
        std::env::set_var("ASTER_REMOTE_CACHE_SIGNATURE_KEY", "srv-test-key");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let fake = Fake::spawn();
            let digest = Digest::of_bytes(b"signed");
            let stored = manifest(&"99".repeat(32), &digest);
            backend(&fake).put_manifest(&stored).unwrap();
            let saved = fake
                .manifests
                .lock()
                .unwrap()
                .get(&stored.key)
                .cloned()
                .unwrap();
            assert_eq!(saved, stored.encode());

            let reader = backend(&fake);
            reader.prefetch(std::slice::from_ref(&stored.key)).unwrap();
            assert_eq!(reader.get_manifest(&stored.key).unwrap().unwrap(), stored);
        }));
        match previous {
            Some(value) => std::env::set_var("ASTER_REMOTE_CACHE_SIGNATURE_KEY", value),
            None => std::env::remove_var("ASTER_REMOTE_CACHE_SIGNATURE_KEY"),
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn signature_key_on_aster_srv_is_reported_not_silently_skipped() {
        assert_eq!(signature_exclusion_notice(false), None);
        let notice = signature_exclusion_notice(true).unwrap();
        assert!(
            notice.contains("ASTER_REMOTE_CACHE_SIGNATURE_KEY"),
            "{notice}"
        );
        assert!(notice.contains("not signature-verified"), "{notice}");
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn a_cleartext_url_on_another_host_is_refused_and_https_and_loopback_are_not() {
        for bad in [
            "http://aster-srv.example.com",
            "http://10.0.0.5:8080",
            "ftp://x",
        ] {
            let error = AsterSrvBackend::from_parts(
                Some(bad),
                Some("acme/app"),
                Some(Credentials::Static("t".into())),
                None,
                None,
            )
            .err()
            .unwrap_or_else(|| panic!("{bad} was accepted"));
            assert!(format!("{error:#}").contains("ASTER_SRV_URL"), "{error:#}");
        }
        for ok in [
            "https://aster-srv.example.com",
            "http://127.0.0.1:9",
            "http://localhost:9",
        ] {
            assert!(
                AsterSrvBackend::from_parts(
                    Some(ok),
                    Some("acme/app"),
                    Some(Credentials::Static("t".into())),
                    None,
                    None,
                )
                .unwrap()
                .is_some(),
                "{ok}"
            );
        }
    }

    #[test]
    fn a_cleartext_blob_base_is_ignored() {
        let backend = AsterSrvBackend::from_parts(
            Some("https://aster-srv.example.com"),
            Some("acme/app"),
            Some(Credentials::Static("t".into())),
            None,
            None,
        )
        .unwrap()
        .unwrap()
        .redirect_gcs_to("http://blobs.example.com");
        assert!(backend.gcs_base.is_none());
    }
}
