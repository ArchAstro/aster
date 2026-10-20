//! HTTP cache backend speaking the bazel-remote REST layout.
//!
//! `GET`/`PUT`/`HEAD <base>/ac/<key>` stores manifests.
//! `GET`/`PUT`/`HEAD <base>/cas/<sha256>` stores blobs, addressed by the sha256 of the
//! uncompressed bytes. Blobs travel uncompressed: a response that names
//! `Content-Encoding: zstd` is rejected rather than stored as if it were raw.
//!
//! Action-cache requests send and accept `application/json`, which is the
//! header pair an unmodified bazel-remote uses for JSON manifests. Without
//! those headers it validates the body as an ActionResult protobuf.
//!
//! A `401` or `403` on a write means the token cannot upload. That write
//! returns an error so the caller does not publish a manifest for a blob that
//! was not stored, and the backend switches to read mode for the rest of the
//! process: later `put_*` calls return `Ok` without contacting the server. A
//! `409` on `PUT` means the object already exists and is success. A `404` on a
//! read is a miss.
//!
//! The bearer token is sent as `Authorization` and is never included in an
//! error, a log line, or [`Debug`].

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::Body;

use super::artifact::{Digest, Manifest};
use super::backend::CacheBackend;
use super::signature;

/// `ASTER_REMOTE_CACHE_URL`. Base URL of a bazel-remote-compatible server.
pub const REMOTE_CACHE_URL_ENV: &str = "ASTER_REMOTE_CACHE_URL";

/// `ASTER_REMOTE_CACHE_TOKEN`. Optional bearer token. Never logged.
pub const REMOTE_CACHE_TOKEN_ENV: &str = "ASTER_REMOTE_CACHE_TOKEN";

const USER_AGENT: &str = concat!("aster/", env!("CARGO_PKG_VERSION"));

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_BODY_TIMEOUT: Duration = Duration::from_secs(300);
const RECV_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const RECV_BODY_TIMEOUT: Duration = Duration::from_secs(3600);

/// bazel-remote action-cache keys are 64 lowercase hex characters.
const AC_KEY_LEN: usize = 64;

/// HTTP cache selected by `ASTER_REMOTE_CACHE=http`.
///
/// When [`super::signature::SIGNATURE_KEY_ENV`] is set, action-cache
/// objects are a JSON envelope signed over [`super::artifact::Manifest::encode`].
/// A missing or wrong signature is a miss. Blobs stay unsigned: the manifest
/// names their digests, and restore checks those digests.
pub struct HttpCache {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
    /// Set after a write is refused. Later writes do not contact the server.
    read_only: AtomicBool,
}

impl std::fmt::Debug for HttpCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpCache")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("read_only", &self.read_only.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl HttpCache {
    /// Client for `base`. `token` is sent as a bearer header when set.
    ///
    /// `base` must be `http://` or `https://`. A trailing slash is stripped.
    /// bazel-remote's default listener is plaintext, so `http://` is accepted;
    /// the token is still never written to an error or a log.
    pub fn new(base: impl AsRef<str>, token: Option<String>) -> Result<Self> {
        let base = base.as_ref().trim().trim_end_matches('/').to_string();
        let rest = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
            .with_context(|| {
                format!("invalid {REMOTE_CACHE_URL_ENV}: expected an http:// or https:// URL")
            })?;
        if rest.is_empty() || rest.starts_with('/') {
            anyhow::bail!("invalid {REMOTE_CACHE_URL_ENV}: expected an http:// or https:// URL");
        }
        let token = token.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        Ok(Self {
            base,
            token,
            agent: build_agent(),
            read_only: AtomicBool::new(false),
        })
    }

    /// `ASTER_REMOTE_CACHE_URL` and `ASTER_REMOTE_CACHE_TOKEN`.
    ///
    /// `None` when the URL is unset or empty: the caller stays local-only.
    /// A present but invalid URL is an error.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(
            std::env::var(REMOTE_CACHE_URL_ENV).ok().as_deref(),
            std::env::var(REMOTE_CACHE_TOKEN_ENV).ok(),
        )
    }

    /// [`Self::from_env`] from already-read values.
    pub fn from_vars(url: Option<&str>, token: Option<String>) -> Result<Option<Self>> {
        let Some(url) = url.map(str::trim).filter(|url| !url.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(Self::new(url, token)?))
    }

    /// Whether a write was refused and this backend no longer uploads.
    pub fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    fn object_url(&self, kind: &str, id: &str) -> String {
        format!("{}/{kind}/{id}", self.base)
    }

    fn get_bytes(&self, url: &str, accept: &str) -> Result<Get> {
        let response = self.send(
            self.request("GET", url, &[("Accept", accept)])?
                .body(ureq::SendBody::none())
                .map_err(|_| anyhow!("build GET request failed"))?,
        )?;
        let status = response.status().as_u16();
        if status == 404 {
            return Ok(Get::Miss);
        }
        if !(200..300).contains(&status) {
            return Err(status_error("GET", status));
        }
        if is_zstd(response.headers()) {
            return Err(anyhow!(
                "GET returned Content-Encoding: zstd; blobs are addressed by uncompressed sha256"
            ));
        }
        Ok(Get::Hit(response))
    }

    fn head_exists(&self, url: &str) -> Result<bool> {
        let response = self.send(
            self.request("HEAD", url, &[])?
                .body(ureq::SendBody::none())
                .map_err(|_| anyhow!("build HEAD request failed"))?,
        )?;
        let status = response.status().as_u16();
        drop(response);
        if status == 404 {
            return Ok(false);
        }
        if !(200..300).contains(&status) {
            return Err(status_error("HEAD", status));
        }
        Ok(true)
    }

    /// `PUT` of an already-buffered body. Manifests are small JSON.
    fn put_bytes(&self, url: &str, bytes: &[u8], headers: &[(&str, &str)]) -> Result<()> {
        if self.is_read_only() {
            return Ok(());
        }
        let request = self
            .request("PUT", url, headers)?
            .header("Content-Length", bytes.len().to_string())
            .body(bytes)
            .map_err(|_| anyhow!("build PUT request failed"))?;
        self.finish_put(self.send(request)?)
    }

    /// `PUT` that reads `reader` as the socket is written.
    ///
    /// No `Content-Length`: `dyn Read` erases `Seek`, and sizing the body
    /// would copy it. ureq sends `Transfer-Encoding: chunked` and reads the
    /// reader in socket-sized chunks. bazel-remote accepts that body.
    fn put_reader(&self, url: &str, reader: &mut dyn Read, headers: &[(&str, &str)]) -> Result<()> {
        if self.is_read_only() {
            return Ok(());
        }
        let request = self
            .request("PUT", url, headers)?
            .body(ureq::SendBody::from_reader(reader))
            .map_err(|_| anyhow!("build PUT request failed"))?;
        self.finish_put(self.send(request)?)
    }

    fn finish_put(&self, response: ureq::http::Response<Body>) -> Result<()> {
        let status = response.status().as_u16();
        drop(response);
        if (200..300).contains(&status) || status == 409 {
            return Ok(());
        }
        if status == 401 || status == 403 {
            self.read_only.store(true, Ordering::Release);
            eprintln!(
                "[aster] Warning: remote cache rejected an upload ({status}); continuing read-only"
            );
            // Err, not Ok: the object was not stored. TieredCache::upload must
            // stop before put_manifest, or a refused blob is published as a hit.
            // The flag makes later put_* calls return Ok without contacting the server.
            return Err(status_error("PUT", status));
        }
        Err(status_error("PUT", status))
    }

    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<ureq::http::request::Builder> {
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("User-Agent", USER_AGENT);
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        Ok(request)
    }

    fn send(
        &self,
        request: ureq::http::Request<impl ureq::AsSendBody>,
    ) -> Result<ureq::http::Response<Body>> {
        self.agent
            .run(request)
            .map_err(|_| anyhow!("remote cache unreachable"))
    }
}

enum Get {
    Miss,
    Hit(ureq::http::Response<Body>),
}

impl CacheBackend for HttpCache {
    fn get_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        validate_ac_key(key)?;
        match self.get_bytes(&self.object_url("ac", key), "application/json")? {
            Get::Miss => Ok(None),
            Get::Hit(response) => {
                let bytes =
                    read_limited(response, MANIFEST_LIMIT).context("read remote manifest")?;
                Ok(signature::open(signature::signing_key().as_deref(), &bytes))
            }
        }
    }

    fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
        validate_ac_key(&manifest.key)?;
        let bytes = signature::seal(signature::signing_key().as_deref(), manifest);
        self.put_bytes(
            &self.object_url("ac", &manifest.key),
            &bytes,
            &[("Content-Type", "application/json")],
        )
    }

    fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>> {
        let mut missing = Vec::new();
        for digest in digests {
            validate_digest(digest)?;
            if !self.head_exists(&self.object_url("cas", digest.as_str()))? {
                missing.push(digest.clone());
            }
        }
        Ok(missing)
    }

    fn get_blob(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool> {
        validate_digest(digest)?;
        match self.get_bytes(
            &self.object_url("cas", digest.as_str()),
            "application/octet-stream",
        )? {
            Get::Miss => Ok(false),
            Get::Hit(response) => {
                io::copy(&mut response.into_body().into_reader(), writer)
                    .context("write remote blob")?;
                Ok(true)
            }
        }
    }

    fn put_blob(&self, digest: &Digest, reader: &mut dyn Read) -> Result<()> {
        validate_digest(digest)?;
        if self.is_read_only() {
            return Ok(());
        }
        self.put_reader(
            &self.object_url("cas", digest.as_str()),
            reader,
            &[("Content-Type", "application/octet-stream")],
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Manifests are small JSON. A larger body is a protocol error, not a blob.
const MANIFEST_LIMIT: u64 = 8 * 1024 * 1024;

fn read_limited(response: ureq::http::Response<Body>, limit: u64) -> Result<Vec<u8>> {
    let mut reader = response.into_body().into_reader().take(limit + 1);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .context("read remote response")?;
    if bytes.len() as u64 > limit {
        anyhow::bail!("remote manifest exceeds {limit} bytes");
    }
    Ok(bytes)
}

fn validate_ac_key(key: &str) -> Result<()> {
    if key.len() != AC_KEY_LEN
        || !key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        anyhow::bail!("remote cache key must be {AC_KEY_LEN} hex characters");
    }
    Ok(())
}

fn validate_digest(digest: &Digest) -> Result<()> {
    let hex = digest.as_str();
    if hex.len() != AC_KEY_LEN
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        anyhow::bail!("remote blob digest must be {AC_KEY_LEN} hex characters");
    }
    Ok(())
}

fn is_zstd(headers: &ureq::http::HeaderMap) -> bool {
    headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("zstd"))
        })
}

/// Status only. The response body and the request URL can echo the token.
fn status_error(method: &str, status: u16) -> anyhow::Error {
    anyhow!("{method} failed: {status}")
}

fn build_agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .root_certs(RootCerts::WebPki)
        .build();
    ureq::Agent::config_builder()
        .https_only(false)
        .http_status_as_error(false)
        .tls_config(tls)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(SEND_REQUEST_TIMEOUT))
        .timeout_send_body(Some(SEND_BODY_TIMEOUT))
        .timeout_recv_response(Some(RECV_RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(RECV_BODY_TIMEOUT))
        .build()
        .new_agent()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_token() {
        let cache =
            HttpCache::new("http://127.0.0.1:9", Some("super-secret-token".into())).unwrap();
        let rendered = format!("{cache:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("super-secret-token"));
    }

    #[test]
    fn from_vars_absent_url_is_unavailable() {
        assert!(HttpCache::from_vars(None, None).unwrap().is_none());
        assert!(HttpCache::from_vars(Some("  "), None).unwrap().is_none());
        assert!(HttpCache::from_vars(Some("not-a-url"), None).is_err());
        assert!(HttpCache::from_vars(Some("http://"), None).is_err());
    }
}
