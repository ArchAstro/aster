//! `aster cache login`: a person's sign-in to an aster-srv.
//!
//! Aster opens the browser at `<ASTER_SRV_URL>/cli-auth`, where the person
//! signs in through the server's identity-aware proxy and confirms. The
//! server then redirects the browser to a one-shot listener Aster holds on
//! `127.0.0.1`, carrying a single-use code. Aster trades the code, plus the
//! PKCE verifier only it knows, for a short-lived token and keeps it in the
//! user's config directory. Later `aster run`s send it as a bearer token
//! to read the shared cache.
//!
//! ```text
//! aster                                browser                    aster-srv
//!   | bind 127.0.0.1:PORT                 |                          |
//!   | open /cli-auth?port&challenge&state |------------------------->| IAP names the person
//!   |                                     |<-- confirm page ---------|
//!   |                                     |--- POST confirm -------->|
//!   |<-- GET /callback?code&state --------|<-- 302 loopback ---------|
//!   | POST /v1/auth/token {code, verifier, port} ------------------->|
//!   |<-- {token, expires_at} ----------------------------------------|
//! ```
//!
//! The token file is `<config>/aster/cache-tokens/<server host>.json`, mode
//! 0600 in a 0700 directory. The token is never printed or logged.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// How close to expiry a stored token is treated as already expired.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// How long `aster cache login` waits for the browser to come back.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The one line printed when there is no usable sign-in.
pub const SIGNED_OUT_MESSAGE: &str = "[aster] not signed in; run aster cache login";

/// A stored sign-in. `token` is a credential; do not print it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredToken {
    /// `scheme://host[:port]` of the server that issued it.
    pub server: String,
    pub token: String,
    /// Unix seconds.
    pub expires_at: u64,
    #[serde(default)]
    pub email: Option<String>,
}

/// `scheme://authority` of `url`, with the path dropped and the host
/// lowercased. `None` unless the scheme is http or https.
fn server_origin(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Where to keep sign-ins: `$XDG_CONFIG_HOME/aster`, else `~/.config/aster`
/// (`%APPDATA%\aster` on Windows).
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(&|name| std::env::var(name).ok())
}

fn config_dir_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let base = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            if cfg!(windows) {
                var("APPDATA").map(PathBuf::from)
            } else {
                var("HOME").map(|home| PathBuf::from(home).join(".config"))
            }
        })?;
    Some(base.join("aster"))
}

/// The token file for `origin`, named by its host (and port).
fn token_path(config_dir: &Path, origin: &str) -> Option<PathBuf> {
    let host = origin.split_once("://")?.1;
    let name: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(config_dir.join("cache-tokens").join(format!("{name}.json")))
}

/// The stored token for `server_url`, if it is still good.
pub fn stored_token(server_url: &str, now: SystemTime) -> Option<StoredToken> {
    load_token(&config_dir()?, server_url, now)
}

fn load_token(config_dir: &Path, server_url: &str, now: SystemTime) -> Option<StoredToken> {
    let origin = server_origin(server_url)?;
    let text = fs::read_to_string(token_path(config_dir, &origin)?).ok()?;
    let stored: StoredToken = serde_json::from_str(&text).ok()?;
    if stored.server != origin || stored.token.is_empty() {
        return None;
    }
    if UNIX_EPOCH + Duration::from_secs(stored.expires_at) <= now + EXPIRY_SKEW {
        return None;
    }
    Some(stored)
}

/// Write `token` so only the owner can read it.
fn store_token(config_dir: &Path, token: &StoredToken) -> Result<PathBuf> {
    let path = token_path(config_dir, &token.server).context("the server URL is not valid")?;
    let dir = path.parent().expect("token files sit in a directory");
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    set_mode(config_dir, 0o700);
    set_mode(dir, 0o700);
    let text = serde_json::to_string(token).context("encode the sign-in")?;
    // Replace through a private temporary file so the token is never
    // readable by others, even briefly.
    let temporary = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    set_mode(&temporary, 0o600);
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, &path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    let _ = (path, mode);
}

/// How to run a sign-in.
pub struct LoginOptions {
    /// Open the system browser at the sign-in URL. Off prints the URL only,
    /// for a machine with no browser or a remote shell.
    pub open_browser: bool,
    /// How long to wait for the browser to come back.
    pub timeout: Duration,
}

/// What a finished sign-in left behind.
#[derive(Debug, PartialEq, Eq)]
pub struct SignedIn {
    pub email: Option<String>,
    pub path: PathBuf,
    pub expires_at: u64,
}

/// Run the sign-in against `server_url`. `announce` is told the URL to open
/// before Aster waits.
pub fn login(
    server_url: &str,
    options: &LoginOptions,
    announce: &mut dyn FnMut(&str),
) -> Result<SignedIn> {
    let config_dir =
        config_dir().context("cannot find a config directory; set XDG_CONFIG_HOME or HOME")?;
    login_in(&config_dir, server_url, options, announce)
}

fn login_in(
    config_dir: &Path,
    server_url: &str,
    options: &LoginOptions,
    announce: &mut dyn FnMut(&str),
) -> Result<SignedIn> {
    let origin = server_origin(server_url)
        .with_context(|| format!("ASTER_SRV_URL must be an http(s) URL, not '{server_url}'"))?;
    let verifier = random_token(32)?;
    let challenge = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token(16)?;

    let listener = TcpListener::bind(("127.0.0.1", 0)).context("cannot listen on 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    let url = format!("{origin}/cli-auth?port={port}&challenge={challenge}&state={state}");
    announce(&url);
    if options.open_browser {
        open_browser(&url);
    }

    let code = wait_for_callback(&listener, &state, options.timeout)?;
    let reply = exchange(&origin, &code, &verifier, port)?;
    let token = StoredToken {
        server: origin,
        token: reply.token,
        expires_at: reply.expires_at,
        email: reply.email,
    };
    let path = store_token(config_dir, &token)?;
    Ok(SignedIn {
        email: token.email,
        path,
        expires_at: token.expires_at,
    })
}

fn random_token(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
    Ok(BASE64URL.encode(buffer))
}

/// Best effort: a missing opener is not an error, the URL is printed anyway.
fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd would read the URL's `&` as a new command.
        let mut command = std::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Serve `GET /callback` until one arrives with the right `state`, and
/// return its `code`. Anything else (a favicon fetch, a wrong state) is
/// answered and ignored, so a stray request cannot end the wait.
fn wait_for_callback(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = handle_callback(stream, state) {
                    return Ok(code);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!(
                        "timed out after {} seconds waiting for the browser; run aster cache login again",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("the loopback listener failed"),
        }
    }
}

fn handle_callback(stream: TcpStream, state: &str) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader
        .by_ref()
        .take(8192)
        .read_line(&mut request_line)
        .ok()?;
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut stream = stream;
    if method != "GET" || path != "/callback" {
        respond(&mut stream, 404, "Not found", "Nothing here.");
        return None;
    }
    let param = |name: &str| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    };
    let (Some(code), Some(got_state)) = (param("code"), param("state")) else {
        respond(
            &mut stream,
            400,
            "Bad request",
            "The sign-in reply is incomplete.",
        );
        return None;
    };
    if got_state != state {
        respond(
            &mut stream,
            400,
            "Bad request",
            "This sign-in reply is for a different request. Run aster cache login again.",
        );
        return None;
    }
    respond(
        &mut stream,
        200,
        "Signed in to aster",
        "You can close this tab and return to your terminal.",
    );
    Some(code)
}

fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <body style=\"font:16px system-ui;margin:3em auto;max-width:32em\">\
         <h1>{title}</h1><p>{message}</p></body>"
    );
    let reason = if status == 200 { "OK" } else { "Error" };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

struct Exchanged {
    token: String,
    expires_at: u64,
    email: Option<String>,
}

fn exchange(origin: &str, code: &str, verifier: &str, port: u16) -> Result<Exchanged> {
    let agent = super::aster_srv::build_agent(Duration::from_secs(15));
    let mut response = agent
        .post(&format!("{origin}/v1/auth/token"))
        .header("Content-Type", "application/json")
        .send(
            json!({"code": code, "verifier": verifier, "port": port})
                .to_string()
                .as_bytes(),
        )
        .context("cannot reach the server to finish signing in")?;
    let status = response.status().as_u16();
    if status != 200 {
        bail!(
            "the server refused the sign-in ({status}); run aster cache login again \
             (if it keeps failing, the server may not have sign-in enabled)"
        );
    }
    let value: Value = response
        .body_mut()
        .read_json()
        .context("cannot read the server's sign-in reply")?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .context("the server's sign-in reply has no token")?
        .to_string();
    let expires_at = value
        .get("expires_at")
        .and_then(Value::as_u64)
        .context("the server's sign-in reply has no expiry")?;
    let email = value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Exchanged {
        token,
        expires_at,
        email,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener as StdListener;
    use std::sync::{Arc, Mutex};

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn origin_keeps_scheme_and_authority_only() {
        assert_eq!(
            server_origin("https://Aster-Srv.Example.com/some/path?x=1").as_deref(),
            Some("https://aster-srv.example.com")
        );
        assert_eq!(
            server_origin("http://127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        for bad in [
            "",
            "ftp://x",
            "aster-srv.example.com",
            "https://",
            "https://u:p@host",
        ] {
            assert_eq!(server_origin(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_config_dir_follows_xdg_then_home() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert_eq!(
            config_dir_from(&env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/home/a")])),
            Some(PathBuf::from("/x/aster"))
        );
        assert_eq!(
            config_dir_from(&env(&[
                ("XDG_CONFIG_HOME", "relative"),
                ("HOME", "/home/a")
            ])),
            if cfg!(windows) {
                None
            } else {
                Some(PathBuf::from("/home/a/.config/aster"))
            }
        );
        assert_eq!(config_dir_from(&env(&[])), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_token_is_private_keyed_by_host_and_ignored_when_stale() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("aster");
        let token = StoredToken {
            server: "https://aster-srv.example.com".into(),
            token: "tok".into(),
            expires_at: 1_700_003_600,
            email: Some("ada@example.com".into()),
        };
        let path = store_token(&config, &token).unwrap();
        assert_eq!(path, config.join("cache-tokens/aster-srv.example.com.json"));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&config), 0o700);

        // Found by any spelling of the same server, until a minute before expiry.
        let url = "https://ASTER-SRV.example.com/";
        assert_eq!(load_token(&config, url, at(1_700_000_000)), Some(token));
        assert_eq!(load_token(&config, url, at(1_700_003_600 - 59)), None);
        // Another server, or a file that names a different server, is not it.
        assert_eq!(
            load_token(&config, "https://other.example.com", at(0)),
            None
        );
        let swapped = StoredToken {
            server: "https://evil.example.com".into(),
            token: "t".into(),
            expires_at: 1_700_003_600,
            email: None,
        };
        fs::write(&path, serde_json::to_string(&swapped).unwrap()).unwrap();
        assert_eq!(load_token(&config, url, at(0)), None);
        // Overwriting keeps the file private.
        store_token(&config, &swapped).unwrap();
        store_token(&config, &swapped).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    /// A one-request token endpoint that records what it was sent.
    fn token_server(reply: &'static str) -> (String, Arc<Mutex<Option<Value>>>) {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(None));
        let seen_c = Arc::clone(&seen);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                line.clear();
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            *seen_c.lock().unwrap() = serde_json::from_slice(&body).ok();
            let status = if reply.is_empty() { 400 } else { 200 };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        });
        (url, seen)
    }

    fn query_param(url: &str, name: &str) -> String {
        url.split(['?', '&'])
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .unwrap()
            .to_string()
    }

    fn get(port: &str, path_and_query: &str) -> String {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        write!(stream, "GET {path_and_query} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    /// Run `login_in` while a thread plays the browser: it gets the URL
    /// Aster announces and calls `browse` with its port and state. Returns
    /// the login result and the announced URL.
    fn login_with_browser(
        config: &Path,
        server: &str,
        browse: impl FnOnce(&str, &str) + Send,
    ) -> (Result<SignedIn>, String) {
        let (announce_tx, announce_rx) = std::sync::mpsc::channel::<String>();
        std::thread::scope(|scope| {
            let browser = scope.spawn(move || {
                let url = announce_rx.recv().unwrap();
                browse(&query_param(&url, "port"), &query_param(&url, "state"));
                url
            });
            let result = login_in(
                config,
                server,
                &LoginOptions {
                    open_browser: false,
                    timeout: Duration::from_secs(20),
                },
                &mut |url| announce_tx.send(url.to_string()).unwrap(),
            );
            (result, browser.join().unwrap())
        })
    }

    #[test]
    fn login_proves_possession_of_the_verifier_and_stores_the_token() {
        let (server, seen) = token_server(
            r#"{"token":"person-token","expires_at":1893456000,"email":"ada@example.com"}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let (signed, url) = login_with_browser(dir.path(), &server, |port, state| {
            // A stray request and callbacks with a missing or wrong state
            // do not end the wait; the right one does.
            assert!(get(port, "/favicon.ico").contains("404"));
            assert!(get(port, "/callback?code=evil&state=wrong").contains("400"));
            assert!(get(port, "/callback?code=good-code&state=").contains("400"));
            assert!(get(port, "/callback?code=good-code").contains("400"));
            let ok = get(port, &format!("/callback?code=good-code&state={state}"));
            assert!(ok.contains("200") && ok.contains("close this tab"), "{ok}");
        });
        let signed = signed.unwrap();

        assert!(
            url.starts_with(&format!("{server}/cli-auth?port=")),
            "{url}"
        );
        // The challenge is the S256 of the verifier sent at exchange.
        let sent = seen.lock().unwrap().clone().unwrap();
        assert_eq!(sent["code"], "good-code");
        let challenge = BASE64URL.encode(Sha256::digest(sent["verifier"].as_str().unwrap()));
        assert_eq!(query_param(&url, "challenge"), challenge);
        assert_eq!(
            sent["port"].as_u64().unwrap().to_string(),
            query_param(&url, "port")
        );
        assert_eq!(signed.email.as_deref(), Some("ada@example.com"));
        let stored = load_token(dir.path(), &server, at(1_700_000_000)).unwrap();
        assert_eq!(stored.token, "person-token");
        assert_eq!(stored.expires_at, 1_893_456_000);
    }

    #[test]
    fn login_gives_up_when_the_browser_never_returns() {
        let dir = tempfile::tempdir().unwrap();
        let error = login_in(
            dir.path(),
            "http://127.0.0.1:9",
            &LoginOptions {
                open_browser: false,
                timeout: Duration::from_millis(200),
            },
            &mut |_| {},
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
        assert!(!dir.path().join("cache-tokens").exists());
    }

    #[test]
    fn a_refused_exchange_stores_nothing_and_says_what_to_do() {
        let (server, _) = token_server("");
        let dir = tempfile::tempdir().unwrap();
        let (result, _) = login_with_browser(dir.path(), &server, |port, state| {
            get(port, &format!("/callback?code=c&state={state}"));
        });
        let text = format!("{:#}", result.unwrap_err());
        assert!(text.contains("refused the sign-in (400)"), "{text}");
        assert!(text.contains("aster cache login"), "{text}");
        assert!(load_token(dir.path(), &server, at(0)).is_none());
    }
}
