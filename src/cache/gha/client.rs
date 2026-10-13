//! Client for the GitHub Actions cache v2 Twirp API and its Azure blob uploads.
//!
//! `ACTIONS_RESULTS_URL` and `ACTIONS_RUNTIME_TOKEN` come from the Actions runner.
//! When either is missing the cache is unavailable, which is not an error: callers
//! fall back to a local cache.

use std::io::{self, Read, Write};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::Body;

const SERVICE: &str = "github.actions.results.api.v1.CacheService";
const USER_AGENT: &str = concat!("aster/", env!("CARGO_PKG_VERSION"));

/// Azure rejects a single Put Blob past a few hundred MiB, and the Actions
/// toolkit stages anything past a small archive as blocks. 32 MiB keeps a
/// typical artifact on one request.
const SINGLE_PUT_LIMIT: u64 = 32 * 1024 * 1024;
/// Put Block chunks. Buffered in full so a retry resends the same bytes.
const BLOCK_SIZE: usize = 8 * 1024 * 1024;

const MAX_ATTEMPTS: u32 = 5;
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

const KEY_MAX_LEN: usize = 512;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_BODY_TIMEOUT: Duration = Duration::from_secs(300);
const RECV_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const RECV_BODY_TIMEOUT: Duration = Duration::from_secs(3600);

/// Outcome of `CreateCacheEntry`. Keys are immutable, so an existing key is
/// success rather than a failure the caller must special-case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateResult {
    pub upload_url: Option<String>,
    pub already_exists: bool,
}

/// GitHub Actions cache v2 client.
pub struct GhaCacheClient {
    results_url: String,
    token: String,
    agent: ureq::Agent,
    sleeper: fn(Duration),
    #[cfg(test)]
    sleep_log: Option<Arc<Mutex<Vec<Duration>>>>,
}

impl GhaCacheClient {
    /// Build a client from the process environment.
    ///
    /// Returns `None` when `ACTIONS_RESULTS_URL` or `ACTIONS_RUNTIME_TOKEN` is
    /// unset or empty. Absence means the cache backend is unavailable.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(
            std::env::var("ACTIONS_RESULTS_URL").ok().as_deref(),
            std::env::var("ACTIONS_RUNTIME_TOKEN").ok().as_deref(),
        )
    }

    /// Build a client from already-read variable values.
    ///
    /// `None` when either value is missing or empty (including whitespace-only).
    pub fn from_vars(results_url: Option<&str>, token: Option<&str>) -> Option<Self> {
        let results_url = non_empty(results_url)?;
        let token = non_empty(token)?;
        Some(Self::new(results_url, token))
    }

    fn new(results_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            results_url: results_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            agent: build_agent(),
            sleeper: std::thread::sleep,
            #[cfg(test)]
            sleep_log: None,
        }
    }

    /// Signed download URL for `key` at `version`, or `None` on a cache miss.
    pub fn lookup(&self, key: &str, version: &str) -> Result<Option<String>> {
        validate_key(key)?;
        let body = json!({ "key": key, "version": version });
        let value = self.twirp("GetCacheEntryDownloadURL", &body)?;
        if !flag(&value, "ok") {
            return Ok(None);
        }
        match string_field(&value, &["signed_download_url", "signedDownloadUrl"]) {
            Some(url) if !url.is_empty() => Ok(Some(url)),
            _ => Ok(None),
        }
    }

    /// Reserve `key`. An existing key returns `already_exists` and no upload URL.
    pub fn create(&self, key: &str, version: &str) -> Result<CreateResult> {
        validate_key(key)?;
        let body = json!({ "key": key, "version": version });
        match self.twirp("CreateCacheEntry", &body) {
            Ok(value) => Ok(interpret_create(&value)),
            Err(error) => {
                if is_already_exists(&error) {
                    Ok(CreateResult {
                        upload_url: None,
                        already_exists: true,
                    })
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Upload `body` (`len` bytes) to the signed Azure blob URL from [`Self::create`].
    ///
    /// At most 32 MiB goes up as one `Put Blob`. Larger archives use 8 MiB
    /// `Put Block` chunks followed by `Put Block List`.
    pub fn upload(&self, upload_url: &str, mut body: impl Read, len: u64) -> Result<()> {
        if len <= SINGLE_PUT_LIMIT {
            let mut buf = Vec::new();
            body.take(len)
                .read_to_end(&mut buf)
                .context("read cache archive for upload")?;
            if buf.len() as u64 != len {
                bail!(
                    "cache archive ended after {} bytes, expected {len}",
                    buf.len()
                );
            }
            self.put_blob(upload_url, &buf)?;
            return Ok(());
        }

        let blocks = usize::try_from(len.div_ceil(BLOCK_SIZE as u64)).unwrap_or(usize::MAX);
        let mut ids = Vec::with_capacity(blocks);
        let mut remaining = len;
        for index in 0..blocks {
            let chunk_len = usize::try_from(remaining.min(BLOCK_SIZE as u64)).unwrap_or(BLOCK_SIZE);
            let mut chunk = vec![0_u8; chunk_len];
            body.read_exact(&mut chunk)
                .with_context(|| format!("read cache block {index}"))?;
            remaining -= chunk_len as u64;
            let id = block_id(index);
            self.put_block(upload_url, &id, &chunk)?;
            ids.push(id);
        }
        self.put_block_list(upload_url, &ids)
    }

    /// Commit an uploaded entry. `size_bytes` is sent as an int64 string.
    pub fn finalize(&self, key: &str, version: &str, size_bytes: u64) -> Result<()> {
        validate_key(key)?;
        let body = json!({
            "key": key,
            "version": version,
            "size_bytes": size_bytes.to_string(),
        });
        let value = self.twirp("FinalizeCacheEntryUpload", &body)?;
        if !flag(&value, "ok") {
            let message = string_field(&value, &["message", "msg"]).unwrap_or_default();
            bail!("finalize cache entry failed: {message}");
        }
        Ok(())
    }

    /// Stream the signed download URL into `writer`.
    pub fn download(&self, url: &str, mut writer: impl Write) -> Result<()> {
        let response = self.send("GET", url, None, &[], false)?;
        io::copy(&mut response.into_body().into_reader(), &mut writer)
            .context("write cache download")?;
        Ok(())
    }

    fn put_blob(&self, url: &str, bytes: &[u8]) -> Result<()> {
        self.send(
            "PUT",
            url,
            Some(bytes),
            &[("x-ms-blob-type", "BlockBlob")],
            false,
        )?;
        Ok(())
    }

    fn put_block(&self, url: &str, block_id: &str, bytes: &[u8]) -> Result<()> {
        let url = format!(
            "{url}{}comp=block&blockid={}",
            if url.contains('?') { '&' } else { '?' },
            percent_encode(block_id),
        );
        self.send("PUT", &url, Some(bytes), &[], false)?;
        Ok(())
    }

    fn put_block_list(&self, url: &str, block_ids: &[String]) -> Result<()> {
        let url = format!(
            "{url}{}comp=blocklist",
            if url.contains('?') { '&' } else { '?' }
        );
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for id in block_ids {
            xml.push_str("<Latest>");
            xml.push_str(&xml_escape(id));
            xml.push_str("</Latest>");
        }
        xml.push_str("</BlockList>");
        self.send(
            "PUT",
            &url,
            Some(xml.as_bytes()),
            &[
                ("Content-Type", "application/xml"),
                ("x-ms-blob-content-type", "application/octet-stream"),
            ],
            false,
        )?;
        Ok(())
    }

    fn twirp(&self, method: &str, body: &Value) -> Result<Value> {
        let url = format!("{}/twirp/{SERVICE}/{method}", self.results_url);
        let payload = serde_json::to_vec(body)?;
        let response = self.send(
            "POST",
            &url,
            Some(&payload),
            &[("Content-Type", "application/json")],
            true,
        )?;
        let text = response
            .into_body()
            .read_to_string()
            .context("read twirp response")?;
        serde_json::from_str(&text).with_context(|| format!("parse {method} response"))
    }

    /// Send `method` and retry 429, 5xx, and transport failures.
    ///
    /// `body` is borrowed so a retry resends the same bytes. `auth` adds the
    /// runtime-token bearer header used by Twirp; blob SAS URLs authenticate
    /// themselves.
    fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        auth: bool,
    ) -> Result<ureq::http::Response<Body>> {
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.once(method, url, body, headers, auth) {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if (200..300).contains(&status) {
                        return Ok(response);
                    }
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    let error_body = response.into_body().read_to_string().unwrap_or_default();
                    let error = status_error(method, status, &error_body);
                    if !is_retryable_status(status) || attempt + 1 == MAX_ATTEMPTS {
                        return Err(error);
                    }
                    let delay = retry_delay(status, retry_after.as_deref(), attempt);
                    self.record_sleep(delay);
                    last_error = Some(error);
                }
                Err(error) => {
                    if attempt + 1 == MAX_ATTEMPTS {
                        return Err(error);
                    }
                    self.record_sleep(backoff(attempt));
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("{method} failed")))
    }

    fn once(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        auth: bool,
    ) -> Result<ureq::http::Response<Body>> {
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("User-Agent", USER_AGENT);
        if auth {
            request = request.header("Authorization", format!("Bearer {}", self.token));
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(bytes) => request.body(bytes),
            None => request.body(&[][..]),
        }
        .map_err(|error| anyhow!("build {method} {url}: {error}"))?;
        self.agent
            .run(request)
            .map_err(|error| anyhow!("{method} {url}: {error}"))
    }

    fn record_sleep(&self, delay: Duration) {
        #[cfg(test)]
        if let Some(log) = &self.sleep_log {
            log.lock().expect("sleep log").push(delay);
        }
        (self.sleeper)(delay);
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > KEY_MAX_LEN {
        bail!(
            "cache key must be 1-{KEY_MAX_LEN} characters, got {}",
            key.len()
        );
    }
    if key.contains(',') {
        bail!("cache key must not contain commas");
    }
    Ok(())
}

fn interpret_create(value: &Value) -> CreateResult {
    if is_already_exists_value(value) || !flag(value, "ok") {
        return CreateResult {
            upload_url: None,
            already_exists: true,
        };
    }
    CreateResult {
        upload_url: string_field(value, &["signed_upload_url", "signedUploadUrl"])
            .filter(|url| !url.is_empty()),
        already_exists: false,
    }
}

fn is_already_exists(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("already_exists") || text.contains("already exists") || text.contains(" 409")
}

fn is_already_exists_value(value: &Value) -> bool {
    let code = string_field(value, &["code"]).unwrap_or_default();
    if code.eq_ignore_ascii_case("already_exists") {
        return true;
    }
    let message = string_field(value, &["message", "msg"]).unwrap_or_default();
    let folded = message.to_ascii_lowercase();
    folded.contains("already exists") || folded.contains("already_exists")
}

fn flag(value: &Value, name: &str) -> bool {
    value.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|field| match field {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    })
}

fn status_error(method: &str, status: u16, body: &str) -> anyhow::Error {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| string_field(&value, &["code"]))
        .map(|code| format!(" {code}"))
        .unwrap_or_default();
    anyhow!("{method} failed: {status}{code}: {body}")
}

fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn retry_delay(status: u16, retry_after: Option<&str>, attempt: u32) -> Duration {
    if status == 429 {
        if let Some(header) = retry_after {
            if let Some(delay) = parse_retry_after(header) {
                return delay.min(RETRY_AFTER_CAP);
            }
        }
    }
    backoff(attempt)
}

fn parse_retry_after(header: &str) -> Option<Duration> {
    let seconds: u64 = header.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

fn backoff(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    BACKOFF_BASE
        .checked_mul(1_u32 << shift)
        .unwrap_or(BACKOFF_CAP)
        .min(BACKOFF_CAP)
}

fn block_id(index: usize) -> String {
    BASE64.encode(format!("block-{index:06}").as_bytes())
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn build_agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        // aws-lc-rs, not ring. Installing ring as the process default would
        // panic ServerConfig::builder() in src/dev/tls.rs, which uses the
        // aws-lc-rs provider tokio-rustls already pulled in.
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
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn no_sleep(_: Duration) {}

    struct FakeServer {
        url: String,
        shutdown: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    struct FakeResponse {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    impl FakeResponse {
        fn json(status: u16, body: Value) -> Self {
            Self {
                status,
                headers: vec![("content-type", "application/json".into())],
                body: serde_json::to_vec(&body).unwrap(),
            }
        }

        fn empty(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }

        fn bytes(status: u16, body: Vec<u8>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body,
            }
        }
    }

    type Handler =
        dyn Fn(&str, &str, &[u8], &HashMap<String, String>) -> FakeResponse + Send + Sync;
    type HttpRequest = (String, String, HashMap<String, String>, Vec<u8>);
    type BlockLog = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    impl FakeServer {
        fn spawn(
            handler: impl Fn(&str, &str, &[u8], &HashMap<String, String>) -> FakeResponse
                + Send
                + Sync
                + 'static,
        ) -> Self {
            let handler: Arc<Handler> = Arc::new(handler);
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = std::sync::mpsc::channel();
            let thread = thread::spawn(move || serve(listener, handler, rx));
            Self {
                url: format!("http://127.0.0.1:{port}"),
                shutdown: Some(tx),
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeServer {
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
        handler: Arc<Handler>,
        shutdown: std::sync::mpsc::Receiver<()>,
    ) {
        listener.set_nonblocking(true).unwrap();
        loop {
            if shutdown.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // macOS/BSD accepted sockets inherit the listener's
                    // non-blocking mode; reads would hit WouldBlock and drop
                    // the request.
                    stream.set_nonblocking(false).unwrap();
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    if let Some((method, path, headers, body)) = read_http(&mut stream) {
                        let response = handler(&method, &path, &body, &headers);
                        let _ = write_http(&mut stream, &response);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }

    fn read_http(stream: &mut std::net::TcpStream) -> Option<HttpRequest> {
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 8192];
        let header_end = loop {
            match stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find_header_end(&buf) {
                        break pos;
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    return None;
                }
                Err(_) => return None,
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut parts = lines.next()?.split(' ');
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < length {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        body.truncate(length);
        Some((method, path, headers, body))
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn write_http(stream: &mut std::net::TcpStream, response: &FakeResponse) -> io::Result<()> {
        let reason = match response.status {
            200 => "OK",
            201 => "Created",
            400 => "Bad Request",
            409 => "Conflict",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
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

    fn client_for(server: &FakeServer) -> GhaCacheClient {
        let mut client = GhaCacheClient::new(server.url.clone(), "runtime-token");
        client.sleeper = no_sleep;
        client
    }

    fn recording_client(server: &FakeServer) -> (GhaCacheClient, Arc<Mutex<Vec<Duration>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_for(server);
        client.sleep_log = Some(Arc::clone(&log));
        (client, log)
    }

    fn query_param<'a>(path: &'a str, name: &str) -> Option<&'a str> {
        let query = path.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then_some(value)
        })
    }

    #[test]
    fn from_vars_absent_is_unavailable() {
        assert!(GhaCacheClient::from_vars(None, None).is_none());
        assert!(GhaCacheClient::from_vars(Some(""), Some("token")).is_none());
        assert!(GhaCacheClient::from_vars(Some("   "), Some("token")).is_none());
        assert!(GhaCacheClient::from_vars(Some("https://results.local"), None).is_none());
        assert!(GhaCacheClient::from_vars(Some("https://results.local"), Some("")).is_none());
        assert!(GhaCacheClient::from_vars(Some("https://results.local"), Some("token")).is_some());
    }

    #[test]
    fn https_builds_rustls_config_without_panicking() {
        let _agent = build_agent();
        // src/dev/tls.rs calls the process-wide builder. A second default
        // provider (ring, from ureq's `rustls` feature) would panic here.
        let _server = rustls::ServerConfig::builder();
    }

    #[test]
    fn lookup_miss_then_full_roundtrip() {
        let blobs: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let store = Arc::clone(&blobs);
        let base = Arc::new(Mutex::new(String::new()));
        let base_for_handler = Arc::clone(&base);
        let server = FakeServer::spawn(move |method, path, body, headers| {
            let path_only = path.split('?').next().unwrap_or(path);
            let origin = base_for_handler.lock().unwrap().clone();
            if path_only.ends_with("/GetCacheEntryDownloadURL") {
                assert_eq!(
                    headers.get("authorization").map(String::as_str),
                    Some("Bearer runtime-token")
                );
                let req: Value = serde_json::from_slice(body).unwrap();
                let key = req["key"].as_str().unwrap();
                let blobs = store.lock().unwrap();
                return if blobs.contains_key(key) {
                    FakeResponse::json(
                        200,
                        json!({
                            "ok": true,
                            "signed_download_url": format!("{origin}/blob/{key}"),
                            "matched_key": key,
                        }),
                    )
                } else {
                    FakeResponse::json(200, json!({"ok": false, "signedDownloadUrl": ""}))
                };
            }
            if path_only.ends_with("/CreateCacheEntry") {
                let req: Value = serde_json::from_slice(body).unwrap();
                let key = req["key"].as_str().unwrap();
                return FakeResponse::json(
                    200,
                    json!({
                        "ok": true,
                        "signedUploadUrl": format!("{origin}/blob/{key}?sig=1"),
                    }),
                );
            }
            if path_only.ends_with("/FinalizeCacheEntryUpload") {
                let req: Value = serde_json::from_slice(body).unwrap();
                assert_eq!(req["size_bytes"].as_str(), Some("11"));
                return FakeResponse::json(200, json!({"ok": true, "entry_id": "7"}));
            }
            if method == "PUT" && path.starts_with("/blob/") {
                assert_eq!(
                    headers.get("x-ms-blob-type").map(String::as_str),
                    Some("BlockBlob")
                );
                let key = path.split('/').nth(2).unwrap().split('?').next().unwrap();
                store.lock().unwrap().insert(key.to_string(), body.to_vec());
                return FakeResponse::empty(201);
            }
            if method == "GET" && path.starts_with("/blob/") {
                let key = path.trim_start_matches("/blob/").split('?').next().unwrap();
                let bytes = store.lock().unwrap().get(key).cloned().unwrap_or_default();
                return FakeResponse::bytes(200, bytes);
            }
            FakeResponse::bytes(404, path.as_bytes().to_vec())
        });
        *base.lock().unwrap() = server.url.clone();

        let client = client_for(&server);
        assert_eq!(client.lookup("app-key", "v1").unwrap(), None);
        let created = client.create("app-key", "v1").unwrap();
        assert!(!created.already_exists);
        let upload_url = created.upload_url.expect("signed upload url");
        let payload = b"hello cache";
        client
            .upload(&upload_url, Cursor::new(&payload[..]), payload.len() as u64)
            .unwrap();
        client
            .finalize("app-key", "v1", payload.len() as u64)
            .unwrap();
        let url = client.lookup("app-key", "v1").unwrap().unwrap();
        let mut downloaded = Vec::new();
        client.download(&url, &mut downloaded).unwrap();
        assert_eq!(downloaded, payload);
    }

    #[test]
    fn create_existing_key_is_already_exists() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = FakeServer::spawn(move |_method, path, _body, _headers| {
            counter.fetch_add(1, Ordering::SeqCst);
            if path.contains("CreateCacheEntry") {
                return FakeResponse::json(
                    409,
                    json!({"code": "already_exists", "msg": "already exists"}),
                );
            }
            FakeResponse::json(200, json!({"ok": false, "message": "cache already exists"}))
        });
        let client = client_for(&server);
        let conflict = client.create("taken", "v1").unwrap();
        assert!(conflict.already_exists);
        assert!(conflict.upload_url.is_none());

        let soft = client.create("taken-soft", "v1");
        // The fake answers every CreateCacheEntry with 409. A second client
        // method is covered by interpret_create below; this call still must
        // not be retried.
        assert!(soft.unwrap().already_exists);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "already-exists is not retried"
        );
    }

    #[test]
    fn create_ok_false_is_already_exists() {
        assert_eq!(
            interpret_create(&json!({"ok": false, "message": "cache already exists"})),
            CreateResult {
                upload_url: None,
                already_exists: true,
            }
        );
        assert_eq!(
            interpret_create(&json!({"code": "already_exists", "msg": "taken"})),
            CreateResult {
                upload_url: None,
                already_exists: true,
            }
        );
    }

    #[test]
    fn upload_large_uses_block_list() {
        let blocks: BlockLog = Arc::new(Mutex::new(Vec::new()));
        let list: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let blocks_h = Arc::clone(&blocks);
        let list_h = Arc::clone(&list);
        let server = FakeServer::spawn(move |method, path, body, _headers| {
            assert_eq!(method, "PUT");
            if path.contains("comp=blocklist") {
                *list_h.lock().unwrap() = String::from_utf8(body.to_vec()).unwrap();
                return FakeResponse::empty(201);
            }
            if path.contains("comp=block") {
                let id = query_param(path, "blockid").unwrap().to_string();
                blocks_h.lock().unwrap().push((id, body.to_vec()));
                return FakeResponse::empty(201);
            }
            FakeResponse::bytes(500, b"expected block upload".to_vec())
        });
        let client = client_for(&server);
        let len = SINGLE_PUT_LIMIT + 3;
        let payload = vec![7_u8; len as usize];
        client
            .upload(
                &format!("{}/blob/big", server.url),
                Cursor::new(payload.clone()),
                len,
            )
            .unwrap();
        let blocks = blocks.lock().unwrap();
        assert_eq!(blocks.len(), 5, "32 MiB + 3 bytes is five 8 MiB blocks");
        assert_eq!(blocks[4].1, payload[BLOCK_SIZE * 4..]);
        let xml = list.lock().unwrap().clone();
        assert!(xml.contains("<BlockList>"));
        assert_eq!(xml.matches("<Latest>").count(), 5);
        assert!(xml.contains(&blocks[0].0));
    }

    #[test]
    fn retries_429_honoring_retry_after() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = FakeServer::spawn(move |_method, _path, _body, _headers| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return FakeResponse {
                    status: 429,
                    headers: vec![("retry-after", "2".into())],
                    body: b"slow down".to_vec(),
                };
            }
            FakeResponse::json(
                200,
                json!({"ok": true, "signed_download_url": "http://blob/x"}),
            )
        });
        let (client, delays) = recording_client(&server);
        let url = client.lookup("k", "v").unwrap();
        assert_eq!(url.as_deref(), Some("http://blob/x"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(delays.lock().unwrap().as_slice(), &[Duration::from_secs(2)]);
    }

    #[test]
    fn retries_5xx_then_succeeds() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = FakeServer::spawn(move |_method, _path, _body, _headers| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                return FakeResponse::bytes(503, b"unavailable".to_vec());
            }
            FakeResponse::json(
                200,
                json!({"ok": true, "signed_upload_url": "http://blob/y"}),
            )
        });
        let (client, delays) = recording_client(&server);
        let created = client.create("k", "v").unwrap();
        assert_eq!(created.upload_url.as_deref(), Some("http://blob/y"));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert_eq!(
            delays.lock().unwrap().as_slice(),
            &[Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[test]
    fn retries_5xx_then_gives_up_after_five_attempts() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = FakeServer::spawn(move |_method, _path, _body, _headers| {
            counter.fetch_add(1, Ordering::SeqCst);
            FakeResponse::bytes(500, b"boom".to_vec())
        });
        let mut client = client_for(&server);
        client.sleeper = no_sleep;
        let error = client.lookup("k", "v").unwrap_err();
        assert!(error.to_string().contains("500"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn non_retryable_4xx_fails_immediately() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = FakeServer::spawn(move |_method, _path, _body, _headers| {
            counter.fetch_add(1, Ordering::SeqCst);
            FakeResponse::json(400, json!({"code": "invalid_argument", "msg": "bad key"}))
        });
        let client = client_for(&server);
        let error = client.finalize("k", "v", 1).unwrap_err();
        assert!(error.to_string().contains("400"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_delay_policy() {
        assert_eq!(retry_delay(429, Some("2"), 0), Duration::from_secs(2));
        assert_eq!(retry_delay(429, Some("120"), 0), RETRY_AFTER_CAP);
        assert_eq!(
            retry_delay(429, Some("not-a-number"), 1),
            Duration::from_secs(2)
        );
        assert_eq!(retry_delay(503, None, 0), Duration::from_secs(1));
        assert_eq!(retry_delay(500, None, 3), Duration::from_secs(8));
        assert_eq!(retry_delay(500, None, 10), BACKOFF_CAP);
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(409));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
    }

    #[test]
    fn rejects_invalid_cache_keys() {
        let client = GhaCacheClient::new("http://127.0.0.1:1", "token");
        assert!(client.lookup("", "v").is_err());
        assert!(client.lookup("a,b", "v").is_err());
        assert!(client.lookup(&"k".repeat(KEY_MAX_LEN + 1), "v").is_err());
        assert!(client.create("a,b", "v").is_err());
    }
}
