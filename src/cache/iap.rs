//! A person's Google IAP token for reading the remote cache.
//!
//! CI already passes IAP with `ASTER_SRV_PROXY_TOKEN`. A laptop or devbox has
//! no shared token: when the proxy challenges, Aster mints a short-lived
//! identity token from that person's `gcloud` login and caches it under
//! `.aster/` until it expires. The file is user-only and never committed
//! (`.aster/` is gitignored). The token is never logged.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// `ASTER_SRV_IAP_CLIENT_ID`: the IAP OAuth client id, the audience of the
/// identity token. A challenge can supply it when this is unset.
pub const IAP_CLIENT_ID_ENV: &str = "ASTER_SRV_IAP_CLIENT_ID";

/// How close to expiry a cached token is treated as already expired.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

const TOKEN_FILE: &str = "iap-token.json";

/// What to run when gcloud is missing or the account is signed out.
pub const GCLOUD_LOGIN_HINT: &str = "\
gcloud auth login, then: gcloud auth print-identity-token --audiences=<IAP OAuth client id>";

/// One line when the remote cannot be reached through IAP.
pub const SIGNED_OUT_MESSAGE: &str = "\
[aster] Warning: not signed in to Google; remote cache skipped. Run `aster cache login`.";

/// A cached identity token. `token` is a credential; do not print it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedToken {
    pub token: String,
    /// Unix seconds when the token expires.
    pub expires_at: u64,
    pub audience: String,
}

/// Where the token file lives: `<workspace>/.aster/iap-token.json`.
pub fn token_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".aster").join(TOKEN_FILE)
}

/// An IAP refusal that names how to mint a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IapChallenge {
    /// OAuth client id from the challenge, when the proxy included one.
    pub client_id: Option<String>,
}

/// Whether `status` plus these headers is Google IAP asking for a token.
///
/// IAP answers an unauthenticated request with 302 or 401 and a
/// `Proxy-Authenticate` or `WWW-Authenticate` bearer challenge. A 200 is the
/// server itself and is not a challenge.
pub fn challenge_from(status: u16, headers: &[(&str, &str)]) -> Option<IapChallenge> {
    if status == 200 || !(status == 302 || status == 401 || status == 403) {
        return None;
    }
    let mut saw_iap = false;
    let mut client_id = None;
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if (name == "proxy-authenticate" || name == "www-authenticate") && is_iap_challenge(value) {
            saw_iap = true;
            if client_id.is_none() {
                client_id = client_id_from(value);
            }
        }
        if name == "location" && is_iap_location(value) {
            saw_iap = true;
            if client_id.is_none() {
                client_id = client_id_from(value);
            }
        }
    }
    if saw_iap {
        Some(IapChallenge { client_id })
    } else {
        None
    }
}

fn is_iap_challenge(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("bearer")
        && (lower.contains("accounts.google.com")
            || lower.contains("iap")
            || lower.contains("client_id")
            || lower.contains("googleusercontent.com"))
}

fn is_iap_location(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("accounts.google.com") || lower.contains("cloud.google.com/iap")
}

/// `client_id` query or challenge parameter, when it looks like an OAuth id.
fn client_id_from(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    for key in ["client_id=", "audience="] {
        let Some(start) = lower.find(key) else {
            continue;
        };
        let rest = value[start + key.len()..].trim();
        let id = if let Some(quoted) = rest.strip_prefix('"') {
            quoted.split('"').next().unwrap_or("").to_string()
        } else {
            let end = rest
                .find(|ch: char| ['&', ' ', ',', '>'].contains(&ch))
                .unwrap_or(rest.len());
            rest[..end].trim().to_string()
        };
        if !id.is_empty()
            && id.len() < 300
            && (id.contains(".apps.googleusercontent.com")
                || id.chars().any(|ch| ch.is_ascii_digit()))
        {
            return Some(id);
        }
    }
    None
}

/// Read a cached token that is still valid for `audience`.
pub fn load_token(path: &Path, audience: &str, now: SystemTime) -> Option<CachedToken> {
    let text = fs::read_to_string(path).ok()?;
    let cached: CachedToken = serde_json::from_str(&text).ok()?;
    if cached.audience != audience || cached.token.is_empty() {
        return None;
    }
    let expires = UNIX_EPOCH + Duration::from_secs(cached.expires_at);
    if expires <= now + EXPIRY_SKEW {
        return None;
    }
    Some(cached)
}

/// Write `token` so only the owner can read it. Creates `.aster/` at 0700.
pub fn store_token(path: &Path, token: &CachedToken) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        set_mode(parent, 0o700);
    }
    let text = serde_json::to_string(token).context("encode IAP token")?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    set_mode(path, 0o600);
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    set_mode(path, 0o600);
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

/// Mint, or reuse, an identity token for `audience`.
///
/// `mint` stands in for `gcloud` in tests. A failure names the login command
/// and does not include the token.
pub fn token_for(
    path: &Path,
    audience: &str,
    now: SystemTime,
    mint: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<String> {
    if let Some(cached) = load_token(path, audience, now) {
        return Ok(cached.token);
    }
    let raw = match mint(audience) {
        Ok(raw) => raw,
        Err(error) => {
            let text = format!("{error:#}");
            if text.contains("gcloud auth login") {
                return Err(error);
            }
            return Err(error.context(format!(
                "cannot mint a Google IAP token. {GCLOUD_LOGIN_HINT}"
            )));
        }
    };
    let token = raw.trim().to_string();
    if token.is_empty() || token.contains('\n') || token.contains(' ') {
        bail!("gcloud did not print an identity token. {GCLOUD_LOGIN_HINT}");
    }
    let expires_at = expiry_of(&token).unwrap_or_else(|| now_secs(now) + 3600);
    store_token(
        path,
        &CachedToken {
            token: token.clone(),
            expires_at,
            audience: audience.to_string(),
        },
    )?;
    Ok(token)
}

/// `gcloud auth print-identity-token --audiences=<audience>`.
pub fn gcloud_mint(audience: &str) -> Result<String> {
    let output = Command::new("gcloud")
        .args(["auth", "print-identity-token", "--audiences", audience])
        .output()
        .with_context(|| format!("gcloud is not installed. {GCLOUD_LOGIN_HINT}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("gcloud failed");
        // gcloud's own text can already name the login command. Don't repeat it.
        if detail.contains("gcloud auth login") {
            bail!("{detail}");
        }
        bail!("{detail}. {GCLOUD_LOGIN_HINT}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Print the login hint, or mint and cache a token.
///
/// Returns the message to print. `Err` is a signed-out or missing-gcloud
/// explanation, not a stack trace.
pub fn login(workspace_root: &Path, audience: &str) -> Result<String> {
    let path = token_path(workspace_root);
    let token = token_for(&path, audience, SystemTime::now(), &mut gcloud_mint)?;
    let _ = token;
    Ok(format!(
        "Signed in to the remote cache. Token cached at {} until it expires.",
        path.display()
    ))
}

/// Audience from the environment, else the challenge.
pub fn audience_from(
    env_client_id: Option<&str>,
    challenge: Option<&IapChallenge>,
) -> Option<String> {
    non_empty(env_client_id).or_else(|| challenge.and_then(|c| c.client_id.clone()))
}

fn non_empty(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn now_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// `exp` from an unsigned JWT payload, when the token has one.
fn expiry_of(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64_url_decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(|exp| exp.as_u64())
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut padded = input.replace('-', "+").replace('_', "/");
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, padded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn jwt(exp: u64) -> String {
        let header = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"alg":"none"}"#,
        );
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            format!(r#"{{"exp":{exp},"email":"a@archastro.ai"}}"#),
        );
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn cached_token_is_reused_until_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iap-token.json");
        let audience = "123.apps.googleusercontent.com";
        let mut mints = 0;
        let minted = token_for(&path, audience, now(), &mut |_audience| {
            mints += 1;
            Ok(jwt(1_700_003_700))
        })
        .unwrap();
        let again = token_for(&path, audience, now(), &mut |_audience| {
            mints += 1;
            Ok(jwt(1_700_003_700))
        })
        .unwrap();
        assert_eq!(minted, again);
        assert_eq!(mints, 1, "a fresh token must not mint again");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be user-only, got {mode:o}");

        // Inside the skew window the cache is stale and a new token is minted.
        let stale_at = UNIX_EPOCH + Duration::from_secs(1_700_003_700 - 30);
        let refreshed = token_for(&path, audience, stale_at, &mut |_audience| {
            mints += 1;
            Ok(jwt(1_700_010_000))
        })
        .unwrap();
        assert_ne!(refreshed, minted);
        assert_eq!(mints, 2);
        let stored: CachedToken =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored.expires_at, 1_700_010_000);
        assert!(!stored.token.is_empty());
    }

    #[test]
    fn expired_token_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iap-token.json");
        store_token(
            &path,
            &CachedToken {
                token: "stale".into(),
                expires_at: 1_700_000_000,
                audience: "aud".into(),
            },
        )
        .unwrap();
        assert!(load_token(&path, "aud", now()).is_none());
        assert!(load_token(&path, "other", now() - Duration::from_secs(120)).is_none());
    }

    #[test]
    fn challenge_names_the_oauth_client() {
        let challenge = challenge_from(
            401,
            &[(
                "Proxy-Authenticate",
                "Bearer realm=\"https://accounts.google.com/\", client_id=\"123.apps.googleusercontent.com\"",
            )],
        )
        .unwrap();
        assert_eq!(
            challenge.client_id.as_deref(),
            Some("123.apps.googleusercontent.com")
        );
        assert!(challenge_from(
            200,
            &[(
                "Proxy-Authenticate",
                "Bearer realm=\"https://accounts.google.com/\""
            )]
        )
        .is_none());
        assert!(challenge_from(401, &[("WWW-Authenticate", "Bearer realm=\"aster\"")]).is_none());
    }

    #[test]
    fn login_explains_when_gcloud_is_missing() {
        let error = token_for(
            Path::new("/no/such"),
            "123.apps.googleusercontent.com",
            now(),
            &mut |_audience| bail!("gcloud is not installed"),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("gcloud auth login"), "{text}");
        assert!(text.contains("print-identity-token"), "{text}");
        assert!(!text.contains("eyJ"), "{text}");
    }
}
