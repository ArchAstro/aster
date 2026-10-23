//! `aster cache login`: a person's sign-in to an aster-srv.
//!
//! Aster opens the browser at `<ASTER_SRV_URL>/cli-auth`, where the person
//! signs in through the server's identity-aware proxy and confirms. Aster
//! gets a single-use code back one of two ways, then trades it, plus the
//! PKCE verifier only it knows, for a short-lived token and keeps it in the
//! user's config directory. Later `aster run`s send it as a bearer token
//! to read the shared cache.
//!
//! 1. **Loopback**: the server redirects the browser to a one-shot listener
//!    Aster holds on `127.0.0.1`.
//! 2. **Copy** (`mode=copy`): for a remote shell (SSH, a devbox), where the
//!    browser runs on another machine and cannot reach that listener. The
//!    server shows the code on a page, the person pastes it into the
//!    terminal, and there is no listener and no port. Copy is chosen by
//!    `--no-browser`, or automatically when [`remote_session`] says there
//!    is probably no usable browser here or the browser fails to open.
//!    `--loopback` forces the first way.
//!
//! ```text
//! loopback
//! aster                                browser                    aster-srv
//!   | bind 127.0.0.1:PORT                 |                          |
//!   | open /cli-auth?port&challenge&state |------------------------->| IAP names the person
//!   |                                     |<-- confirm page ---------|
//!   |                                     |--- POST confirm -------->|
//!   |<-- GET /callback?code&state --------|<-- 302 loopback ---------|
//!   | POST /v1/auth/token {code, verifier, port} ------------------->|
//!   |<-- {token, expires_at} ----------------------------------------|
//!
//! copy
//! aster (remote shell)                 browser (anywhere)         aster-srv
//!   | print /cli-auth?mode=copy&challenge&state                      |
//!   |                                     |--- GET, POST confirm --->|
//!   |                                     |<-- page with the code ---|
//!   | read the pasted code from stdin     |                          |
//!   | POST /v1/auth/token {code, verifier} (no port) --------------->|
//!   |<-- {token, expires_at} ----------------------------------------|
//! ```
//!
//! The `state` is not a secret in copy mode. The server shows its first 8
//! characters on the page and Aster prints them (the request id), so the
//! person can see the page belongs to the command they ran. What stops
//! someone else's code from signing the person in is the PKCE verifier:
//! the code only exchanges with the verifier of the request that made it.
//!
//! The token file is `<config>/aster/cache-tokens/<server host>.json`, mode
//! 0600 in a 0700 directory. Aster refuses to read a token file that other
//! users can read. The token is never printed or logged. `aster cache
//! logout` deletes the file and has the server refuse every token the
//! person was issued before.

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

use crate::srv_url::{checked_origin, is_secure_origin, server_origin};

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
    let origin = server_origin(server_url).filter(|origin| is_secure_origin(origin))?;
    let path = token_path(config_dir, &origin)?;
    if readable_by_others(&path) {
        eprintln!(
            "[aster] ignoring {}: other users can read it; run aster cache login again",
            path.display()
        );
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    let stored: StoredToken = serde_json::from_str(&text).ok()?;
    if stored.server != origin || stored.token.is_empty() {
        return None;
    }
    if UNIX_EPOCH + Duration::from_secs(stored.expires_at) <= now + EXPIRY_SKEW {
        return None;
    }
    Some(stored)
}

/// Whether `path` exists and its group or world bits are set. A token file
/// like that may already have been read by someone else, so it is not used.
fn readable_by_others(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o077 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
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
    restrict_to_owner(&temporary)?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, &path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// Windows ignores mode bits, so the file gets an ACL that names only the
/// current user. If that cannot be set the token is not stored.
#[cfg(windows)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    let user = std::env::var("USERNAME").context("cannot tell which user owns the token file")?;
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:(F)"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("cannot run icacls to make the token file private")?;
    if !status.success() {
        let _ = fs::remove_file(path);
        bail!(
            "cannot make {} private to you; not storing the sign-in",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    let _ = (path, mode);
}

/// How the code gets back to Aster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    /// Loopback when a browser can plausibly be used here, else copy.
    Auto,
    /// Always the loopback redirect.
    Loopback,
    /// Always a pasted code.
    Copy,
}

/// How to run a sign-in.
pub struct LoginOptions {
    pub mode: LoginMode,
    /// Open the system browser at the sign-in URL (loopback only). Off
    /// prints the URL only.
    pub open_browser: bool,
    /// How long to wait for the browser to come back (loopback only; a
    /// pasted code waits for the person, and the server's code expires).
    pub timeout: Duration,
}

/// Why a pasted code is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeReason {
    /// The person asked (`--no-browser`).
    Requested,
    /// This looks like a remote shell or a machine with no display.
    RemoteSession,
    /// The system browser could not be started.
    BrowserFailed,
}

/// What `announce` is told before Aster waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// A browser was started; the callback arrives on the loopback port.
    OpenedBrowser,
    /// The person opens the URL; the callback arrives on the loopback port.
    LoopbackUrl,
    /// The person opens the URL anywhere and pastes the code shown there.
    Code(CodeReason),
}

pub struct Announcement<'a> {
    pub url: &'a str,
    pub flow: Flow,
    /// The first characters of the state, which the sign-in page shows.
    pub request_id: &'a str,
    /// `ABCD-EFGH`: the page asks the person to confirm only if the
    /// terminal shows this.
    pub user_code: &'a str,
}

/// What a finished sign-in left behind.
#[derive(Debug, PartialEq, Eq)]
pub struct SignedIn {
    pub email: Option<String>,
    pub path: PathBuf,
    pub expires_at: u64,
}

/// Whether this looks like a shell where a browser cannot reach Aster's
/// loopback port: an SSH session (`SSH_CONNECTION`, `SSH_CLIENT` or
/// `SSH_TTY` set), or Linux with neither `DISPLAY` nor `WAYLAND_DISPLAY`.
pub fn remote_session(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| set(name))
    {
        return true;
    }
    cfg!(target_os = "linux") && !set("DISPLAY") && !set("WAYLAND_DISPLAY")
}

/// Run the sign-in against `server_url`. `announce` is told the URL before
/// Aster waits; `input` is where a pasted code is read from.
pub fn login(
    server_url: &str,
    options: &LoginOptions,
    announce: &mut dyn FnMut(&Announcement),
    input: &mut dyn BufRead,
) -> Result<SignedIn> {
    let config_dir =
        config_dir().context("cannot find a config directory; set XDG_CONFIG_HOME or HOME")?;
    login_in(&config_dir, server_url, options, announce, input)
}

/// This machine's name for the sign-in page, so the person can tell whether
/// the request is theirs. Best effort; only characters the server accepts.
fn host_hint() -> Option<String> {
    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| fs::read_to_string("/proc/sys/kernel/hostname").ok())?;
    let name: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn login_in(
    config_dir: &Path,
    server_url: &str,
    options: &LoginOptions,
    announce: &mut dyn FnMut(&Announcement),
    input: &mut dyn BufRead,
) -> Result<SignedIn> {
    let origin = checked_origin(server_url)?;
    let verifier = random_token(32)?;
    let user_code = random_user_code()?;
    let shown_code = format!("{}-{}", &user_code[..4], &user_code[4..]);
    let challenge = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token(16)?;
    let request_id = &state[..8];

    let mut reason = match options.mode {
        LoginMode::Copy => Some(CodeReason::Requested),
        LoginMode::Auto if options.open_browser && remote_session(&|n| std::env::var(n).ok()) => {
            Some(CodeReason::RemoteSession)
        }
        LoginMode::Auto if !options.open_browser => Some(CodeReason::Requested),
        _ => None,
    };
    let listener = if reason.is_none() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("cannot listen on 127.0.0.1")?;
        let port = listener.local_addr()?.port();
        let url = format!(
            "{origin}/cli-auth?port={port}&challenge={challenge}&state={state}&challenge_method=S256&user_code={user_code}"
        );
        let opened = options.open_browser && open_browser(&url);
        if options.open_browser && !opened && options.mode == LoginMode::Auto {
            reason = Some(CodeReason::BrowserFailed);
            None
        } else {
            let flow = if opened {
                Flow::OpenedBrowser
            } else {
                Flow::LoopbackUrl
            };
            announce(&Announcement {
                url: &url,
                flow,
                request_id,
                user_code: &shown_code,
            });
            Some((listener, port, url))
        }
    } else {
        None
    };

    let (code, port) = match (listener, reason) {
        (Some((listener, port, _)), _) => (
            wait_for_callback(&listener, &state, options.timeout)?,
            Some(port),
        ),
        (None, Some(reason)) => {
            let mut url = format!(
                "{origin}/cli-auth?mode=copy&challenge={challenge}&state={state}&challenge_method=S256&user_code={user_code}"
            );
            if let Some(host) = host_hint() {
                url.push_str(&format!("&host={host}"));
            }
            announce(&Announcement {
                url: &url,
                flow: Flow::Code(reason),
                request_id,
                user_code: &shown_code,
            });
            (read_pasted_code(input)?, None)
        }
        (None, None) => unreachable!("a loopback listener or a copy reason is always set"),
    };
    let reply = exchange(&origin, &code, &verifier, &user_code, port)?;
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

/// What `aster cache logout` did.
#[derive(Debug, PartialEq, Eq)]
pub struct SignedOut {
    /// The token file that was deleted, if there was one.
    pub deleted: Option<PathBuf>,
    /// Whether the server now refuses every earlier token. `Err` holds why
    /// not; the local file is deleted either way.
    pub revoked: std::result::Result<(), String>,
}

/// Sign out of `server_url`: ask the server to refuse every token issued to
/// this person so far, then delete the local token file.
pub fn logout(server_url: &str) -> Result<SignedOut> {
    let config_dir =
        config_dir().context("cannot find a config directory; set XDG_CONFIG_HOME or HOME")?;
    logout_in(&config_dir, server_url)
}

fn logout_in(config_dir: &Path, server_url: &str) -> Result<SignedOut> {
    let origin = checked_origin(server_url)?;
    let path = token_path(config_dir, &origin).context("the server URL is not valid")?;
    if !path.exists() {
        return Ok(SignedOut {
            deleted: None,
            revoked: Err("this machine was not signed in".into()),
        });
    }
    // A file other users can read is not trusted or sent anywhere.
    let revoked = match load_token(config_dir, &origin, SystemTime::UNIX_EPOCH) {
        Some(stored) => revoke(&origin, &stored.token),
        None => Err("no usable token on this machine to revoke".into()),
    };
    fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))?;
    Ok(SignedOut {
        deleted: Some(path),
        revoked,
    })
}

fn revoke(origin: &str, token: &str) -> std::result::Result<(), String> {
    let agent = super::aster_srv::build_agent(origin, Duration::from_secs(15));
    let response = agent
        .post(&format!("{origin}/v1/auth/revoke"))
        .header("Authorization", &format!("Bearer {token}"))
        .send(&[][..])
        .map_err(|error| format!("cannot reach the server: {error}"))?;
    match response.status().as_u16() {
        200 => Ok(()),
        // Already expired or revoked: nothing is left to refuse.
        401 => Ok(()),
        status => Err(format!("the server answered {status}")),
    }
}

/// The code the person pasted: one line, trimmed.
fn read_pasted_code(input: &mut dyn BufRead) -> Result<String> {
    let mut line = String::new();
    let read = (&mut *input)
        .take(1024)
        .read_line(&mut line)
        .context("cannot read the code")?;
    let code = line.trim();
    if read == 0 {
        bail!("no code was entered; run aster cache login again");
    }
    if code.is_empty()
        || code.len() > 128
        || !code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("that is not a sign-in code; copy the code shown on the page and run aster cache login again");
    }
    Ok(code.to_string())
}

/// Eight characters of the server's user-code alphabet (5 bits each).
fn random_user_code() -> Result<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
    // 32 symbols divide 256 evenly, so there is no modulo bias.
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect())
}

fn random_token(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
    Ok(BASE64URL.encode(buffer))
}

/// Start the system browser. False when it cannot be started, which sends
/// an automatic sign-in to the pasted-code flow.
fn open_browser(url: &str) -> bool {
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
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
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

fn exchange(
    origin: &str,
    code: &str,
    verifier: &str,
    user_code: &str,
    port: Option<u16>,
) -> Result<Exchanged> {
    let agent = super::aster_srv::build_agent(origin, Duration::from_secs(15));
    let mut response = agent
        .post(&format!("{origin}/v1/auth/token"))
        .header("Content-Type", "application/json")
        .send(
            // A copy-mode code has no port; the server requires the mode to match.
            match port {
                Some(port) => {
                    json!({"code": code, "verifier": verifier, "user_code": user_code, "port": port})
                }
                None => json!({"code": code, "verifier": verifier, "user_code": user_code}),
            }
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
                    mode: LoginMode::Loopback,
                    open_browser: false,
                    timeout: Duration::from_secs(20),
                },
                &mut |announcement| announce_tx.send(announcement.url.to_string()).unwrap(),
                &mut &b""[..],
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
        // The user code the page shows is the one the exchange presents.
        assert_eq!(sent["user_code"], query_param(&url, "user_code").as_str());
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
                mode: LoginMode::Loopback,
                open_browser: false,
                timeout: Duration::from_millis(200),
            },
            &mut |_| {},
            &mut &b""[..],
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

    #[test]
    fn a_pasted_code_signs_in_with_no_port_and_the_user_code() {
        let (server, seen) = token_server(
            r#"{"token":"copy-token","expires_at":1893456000,"email":"ada@example.com"}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let mut announced = None;
        let signed = login_in(
            dir.path(),
            &server,
            &LoginOptions {
                mode: LoginMode::Copy,
                open_browser: false,
                timeout: Duration::from_secs(1),
            },
            &mut |a| {
                announced = Some((a.url.to_string(), a.flow, a.user_code.to_string()));
            },
            &mut &b"  pasted-code_123 \n"[..],
        )
        .unwrap();
        let (url, flow, shown) = announced.unwrap();
        assert_eq!(flow, Flow::Code(CodeReason::Requested));
        assert!(url.contains("/cli-auth?mode=copy&challenge="), "{url}");
        assert!(!url.contains("port="), "copy mode has no port: {url}");
        assert!(url.contains("challenge_method=S256"), "{url}");
        let user_code = query_param(&url, "user_code");
        assert_eq!(shown, format!("{}-{}", &user_code[..4], &user_code[4..]));
        let sent = seen.lock().unwrap().clone().unwrap();
        assert_eq!(sent["code"], "pasted-code_123");
        assert_eq!(sent["user_code"], user_code.as_str());
        assert!(sent.get("port").is_none(), "{sent}");
        let challenge = BASE64URL.encode(Sha256::digest(sent["verifier"].as_str().unwrap()));
        assert_eq!(query_param(&url, "challenge"), challenge);
        assert_eq!(signed.email.as_deref(), Some("ada@example.com"));
        assert_eq!(
            load_token(dir.path(), &server, at(1_700_000_000))
                .unwrap()
                .token,
            "copy-token"
        );
    }

    #[test]
    fn a_missing_or_garbled_paste_stores_nothing() {
        for (input, expect) in [
            (&b""[..], "no code was entered"),
            (&b"\n"[..], "not a sign-in code"),
            (&b"<script>\n"[..], "not a sign-in code"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let error = login_in(
                dir.path(),
                "http://127.0.0.1:9",
                &LoginOptions {
                    mode: LoginMode::Copy,
                    open_browser: false,
                    timeout: Duration::from_secs(1),
                },
                &mut |_| {},
                &mut { input },
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expect), "{error:#}");
            assert!(!dir.path().join("cache-tokens").exists());
        }
    }

    #[test]
    fn remote_shells_and_displayless_linux_choose_a_code() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert!(remote_session(&env(&[
            ("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22"),
            ("DISPLAY", ":0")
        ])));
        assert!(remote_session(&env(&[
            ("SSH_TTY", "/dev/pts/1"),
            ("DISPLAY", ":0")
        ])));
        assert!(remote_session(&env(&[
            ("SSH_CLIENT", "x"),
            ("DISPLAY", ":0")
        ])));
        assert!(!remote_session(&env(&[
            ("SSH_CONNECTION", ""),
            ("DISPLAY", ":0")
        ])));
        assert!(!remote_session(&env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        // With no display variable only Linux is treated as headless: macOS
        // and Windows have a window system without them.
        assert_eq!(remote_session(&env(&[])), cfg!(target_os = "linux"));
    }

    #[test]
    fn plain_http_is_for_this_machine_only() {
        for ok in [
            "https://aster-srv.example.com",
            "http://127.0.0.1:8080",
            "http://localhost:3000",
            "http://[::1]:3000",
        ] {
            assert!(is_secure_origin(ok), "{ok}");
        }
        for bad in [
            "http://aster-srv.example.com",
            "http://10.0.0.5:8080",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com",
        ] {
            assert!(!is_secure_origin(bad), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let error = login_in(
            dir.path(),
            "http://aster-srv.example.com",
            &LoginOptions {
                mode: LoginMode::Copy,
                open_browser: false,
                timeout: Duration::from_secs(1),
            },
            &mut |_| panic!("nothing is announced for a cleartext server"),
            &mut &b"code\n"[..],
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("must be https"), "{error:#}");
        // A stored token for such a server is never handed to the cache client.
        let config = dir.path().join("aster");
        let token = StoredToken {
            server: "http://aster-srv.example.com".into(),
            token: "t".into(),
            expires_at: 1_893_456_000,
            email: None,
        };
        store_token(&config, &token).unwrap();
        assert_eq!(
            load_token(&config, "http://aster-srv.example.com", at(0)),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_token_file_other_users_can_read_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("aster");
        let token = StoredToken {
            server: "https://aster-srv.example.com".into(),
            token: "t".into(),
            expires_at: 1_893_456_000,
            email: None,
        };
        let path = store_token(&config, &token).unwrap();
        assert!(load_token(&config, &token.server, at(0)).is_some());
        for mode in [0o640, 0o604, 0o644, 0o660] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(load_token(&config, &token.server, at(0)), None, "{mode:o}");
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_token(&config, &token.server, at(0)).is_some());
    }

    #[test]
    fn logout_revokes_with_the_bearer_then_deletes_the_file() {
        let (server, _) = token_server(r#"{"revoked":true}"#);
        let dir = tempfile::tempdir().unwrap();
        let token = StoredToken {
            server: server.clone(),
            token: "person-token".into(),
            expires_at: 1_893_456_000,
            email: None,
        };
        let path = store_token(dir.path(), &token).unwrap();
        let out = logout_in(dir.path(), &server).unwrap();
        assert_eq!(out.deleted.as_deref(), Some(path.as_path()));
        assert_eq!(out.revoked, Ok(()));
        assert!(!path.exists());
        // Signed out already: nothing deleted, and it says so.
        let again = logout_in(dir.path(), &server).unwrap();
        assert_eq!(again.deleted, None);
        assert!(again.revoked.is_err());
    }

    #[test]
    fn logout_deletes_the_file_even_when_the_server_cannot_be_told() {
        let dir = tempfile::tempdir().unwrap();
        let server = "http://127.0.0.1:9".to_string();
        let token = StoredToken {
            server: server.clone(),
            token: "t".into(),
            expires_at: 1_893_456_000,
            email: None,
        };
        let path = store_token(dir.path(), &token).unwrap();
        let out = logout_in(dir.path(), &server).unwrap();
        assert!(out.revoked.is_err());
        assert!(!path.exists(), "the local token is gone either way");
    }
}
