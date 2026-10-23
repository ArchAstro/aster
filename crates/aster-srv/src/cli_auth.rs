//! Sign-in for `aster cache login`: an OAuth-style authorization-code flow
//! with PKCE (S256 only), in one of two modes.
//!
//! ```text
//! loopback (browser and CLI on one machine)
//! aster                       browser (behind IAP)                aster-srv
//!   | listen 127.0.0.1:PORT          |                                |
//!   | open /cli-auth?port&challenge&state ----------------------->   | GET: IAP email, confirm page + MAC
//!   |                                | POST /cli-auth (form) ------>  | check IAP, origin, MAC; store hash(code)
//!   |  <-- 302 http://127.0.0.1:PORT/callback?code&state ----------   |
//!   | POST /v1/auth/token {code, verifier, port} ------------------>  | DELETE code row, check PKCE + port
//!   |  <-- {token, expires_at} --------------------------------------|
//!
//! copy (SSH, devbox: the browser cannot reach the CLI)
//! aster (remote shell)        browser (anywhere)                  aster-srv
//!   | print /cli-auth?mode=copy&challenge&state                      |
//!   |                                | GET, then POST /cli-auth ---->  | as above, but no port
//!   |                                |  <-- 200 page showing the code  |
//!   | person pastes the code into the terminal                       |
//!   | POST /v1/auth/token {code, verifier} ------------------------>  | DELETE code row, check PKCE, no port
//! ```
//!
//! `/cli-auth` sits behind IAP, which is how the person is known. `/v1/auth/token`
//! does not: the CLI has no IAP session, and the code plus PKCE verifier are
//! its credential. A code is 256 random bits, kept only as its SHA-256, valid
//! for 60 seconds in both modes and spent by the `DELETE` that reads it, so it
//! works once even with several replicas. Every refusal at exchange is the
//! same `400 invalid_grant`.
//!
//! The CLI also makes an 8-character user code, prints it in the terminal and
//! sends it in the URL. The confirmation page shows it, so the person confirms
//! only a request their own terminal displays; it is in the form's MAC and the
//! stored row, and the exchange must present it. `/cli-auth` also refuses an
//! IAP assertion older than five minutes, so a leaked header cannot mint codes.
//!
//! A code is bound to the person, the PKCE challenge, the user code and the mode. The mode is
//! the stored port: a loopback code carries its port (1024 or more), a copy
//! code carries 0. The exchange must send the same port (none means 0), so a
//! copy code is useless to a loopback exchange and the reverse, and the
//! confirmation form's MAC covers the mode so a loopback form cannot be
//! resubmitted to produce a copy code.
//!
//! The loopback redirect goes to `http://127.0.0.1:<port>/callback` and
//! nowhere else. Copy mode never redirects: the page shows the code, never
//! a token. The confirmation page asks before minting, shows where the
//! request came from, and its form carries a MAC over the person, challenge,
//! mode, port, state and expiry, so a page for one person cannot be submitted
//! for another (login CSRF). The POST must also come from this server's own
//! origin.

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::audit;
use crate::db::Migration;
use crate::iap::{Viewer, ViewerError};
use crate::person::random_token;
use crate::test_results::query::parse_query;
use crate::ui::html::{bare_page, escape};
use crate::{App, Reply};

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        feature: "auth",
        version: 1,
        sql: r#"
CREATE SCHEMA IF NOT EXISTS auth;

-- Only the SHA-256 of a code is stored: a database read cannot redeem one.
CREATE TABLE auth.cli_codes (
    code_hash bytea PRIMARY KEY,
    email text NOT NULL,
    challenge text NOT NULL,
    port int NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE INDEX cli_codes_expires_at ON auth.cli_codes (expires_at);
"#,
    },
    // Person tokens carry the stable IAP subject, so a code remembers it.
    // A code minted before this column has none and is refused at exchange.
    Migration {
        feature: "auth",
        version: 2,
        sql: "ALTER TABLE auth.cli_codes ADD COLUMN subject text;",
    },
    // Signing out everywhere: tokens issued to `subject` (the stable IAP
    // subject) before `not_before` are refused.
    Migration {
        feature: "auth",
        version: 3,
        sql: r#"
CREATE TABLE auth.revocations (
    subject text PRIMARY KEY,
    not_before timestamptz NOT NULL
);
"#,
    },
    // The code a person confirms by eye against their terminal. Rows from
    // before this column have none and can never be redeemed.
    Migration {
        feature: "auth",
        version: 4,
        sql: "ALTER TABLE auth.cli_codes ADD COLUMN user_code text;",
    },
];

/// How long a code can be redeemed, in both modes. A pasted code gets no
/// longer than a loopback one: the person copies it right after Authorize.
const CODE_SECONDS: i64 = 60;
/// How long the confirmation page's form is good for.
const FORM_SECONDS: i64 = 600;
/// The largest form body.
pub const MAX_FORM_BYTES: usize = 8 * 1024;
const MIN_PORT: u16 = 1024;
/// How old an IAP assertion may be on `/cli-auth`. IAP signs a fresh one
/// per request, so a leaked header is useless for minting after this.
const IAP_MAX_AGE_SECONDS: i64 = 300;
/// The user code alphabet: no `0`, `O`, `1` or `I`.
const USER_CODE_ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// What a browser request carried that sign-in looks at.
#[derive(Debug)]
pub struct Browser {
    /// The IAP assertion header.
    pub assertion: Option<String>,
    pub origin: Option<String>,
    pub referer: Option<String>,
    /// The caller as [`crate::limits::client_ip`] resolves it: the one trust
    /// model for `X-Forwarded-For`.
    pub client_ip: std::net::IpAddr,
    pub user_agent: Option<String>,
}

/// How the code gets back to the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A redirect to the CLI's loopback port.
    Loopback,
    /// A page that shows the code, for a shell the browser cannot reach.
    Copy,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Loopback => "loopback",
            Mode::Copy => "copy",
        }
    }
}

/// What the CLI asks for.
struct Request {
    mode: Mode,
    /// The loopback port; 0 in copy mode.
    port: u16,
    challenge: String,
    state: String,
    /// The short code the CLI printed, 8 characters of [`USER_CODE_ALPHABET`].
    user_code: String,
    /// The CLI machine's name, shown to the person (a hint, not trusted).
    host: Option<String>,
}

fn url_safe(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_request(pairs: &[(String, String)]) -> Result<Request, &'static str> {
    let find = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    };
    // PKCE `plain` would put the verifier itself in the URL.
    if !matches!(find("challenge_method"), "" | "S256") {
        return Err("Only the S256 challenge method is supported.");
    }
    let mode = match find("mode") {
        "" | "loopback" => Mode::Loopback,
        "copy" => Mode::Copy,
        _ => return Err("The mode must be loopback or copy."),
    };
    let port = find("port");
    let port = match mode {
        Mode::Loopback => (!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
            .then(|| port.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port >= MIN_PORT)
            .ok_or("The port must be a number from 1024 to 65535.")?,
        Mode::Copy if port.is_empty() => 0,
        Mode::Copy => return Err("Copy mode takes no port."),
    };
    let host = find("host");
    if host.len() > 64
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err("The host must be at most 64 letters, digits, '-', '_' or '.'.");
    }
    let challenge = find("challenge");
    // A SHA-256 as unpadded base64url.
    if challenge.len() != 43 || !url_safe(challenge) {
        return Err("The challenge must be a 43-character base64url SHA-256.");
    }
    let state = find("state");
    if state.is_empty() || state.len() > 128 || !url_safe(state) {
        return Err("The state must be 1 to 128 base64url characters.");
    }
    let user_code = find("user_code");
    if user_code.len() != 8 || !user_code.chars().all(|c| USER_CODE_ALPHABET.contains(c)) {
        return Err("The user code must be 8 characters, letters and digits without 0, O, 1, I.");
    }
    Ok(Request {
        user_code: user_code.to_string(),
        mode,
        port,
        host: (!host.is_empty()).then(|| host.to_string()),
        challenge: challenge.to_string(),
        state: state.to_string(),
    })
}

fn page(status: u16, title: &str, message: &str) -> Reply {
    Reply::html(
        status,
        bare_page(title, &format!("<p>{}</p>", escape(message))),
    )
}

/// Whether sign-in is on: the keys and public URL are set, the kill switch
/// (`ASTER_SRV_PERSON_TOKENS=off`) is not thrown, and IAP (or the
/// development viewer) can name a person.
fn enabled(app: &App) -> bool {
    app.person_tokens.is_some() && app.iap.enabled()
}

async fn viewer(app: &App, assertion: Option<&str>) -> Result<Viewer, Reply> {
    match app.iap.verify(assertion).await {
        Ok(_) if !assertion_is_recent(assertion) => Err(page(
            401,
            "Sign in",
            "Your sign-in is too old. Reload this page to sign in again.",
        )),
        Ok(viewer) if !viewer.email.contains('\0') => Ok(viewer),
        Ok(_) => {
            audit::auth_failure(None, "iap_invalid", "the viewer's email holds a NUL");
            Err(page(401, "Sign in", "The sign-in assertion is not valid."))
        }
        Err(error @ ViewerError::Forbidden(_)) => {
            audit::auth_failure(None, "iap_forbidden", "the viewer's domain is not allowed");
            Err(page(403, "Not allowed", &error.to_string()))
        }
        Err(error) => {
            audit::auth_failure(None, "iap_invalid", &error.to_string());
            Err(page(401, "Sign in", &error.to_string()))
        }
    }
}

/// `scheme://authority` of `url`.
fn origin_of(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&url[..scheme.len() + 3 + end])
}

/// Whether a form POST came from `public_url`'s own pages. Browsers send
/// `Origin` on every form POST, and `Referer` when they do not; a request
/// with neither, or with another site's, is refused.
fn from_own_origin(public_url: &str, browser: &Browser) -> bool {
    let Some(own) = origin_of(public_url) else {
        return false;
    };
    match (&browser.origin, &browser.referer) {
        (Some(origin), _) => origin == own,
        (None, Some(referer)) => referer
            .strip_prefix(own)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#'])),
        (None, None) => false,
    }
}

fn short_user_agent(user_agent: Option<&str>) -> Option<String> {
    let text: String = user_agent?
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    (!text.is_empty()).then_some(text)
}

/// The first characters of the state, which the CLI prints too so the
/// person can see the page belongs to the command they just ran.
fn request_id(state: &str) -> &str {
    &state[..state.len().min(8)]
}

/// The fields the confirmation MAC covers, in order.
fn mac_fields<'a>(
    email: &'a str,
    request: &'a Request,
    port: &'a str,
    expiry: &'a str,
) -> [&'a str; 7] {
    [
        email,
        &request.challenge,
        request.mode.name(),
        port,
        &request.state,
        &request.user_code,
        expiry,
    ]
}

/// `ABCD-EFGH`.
fn display_user_code(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..])
}

/// The IAP assertion's age check. The signature is already verified by
/// [`App::iap`]; this reads its `iat`. No assertion means the development
/// viewer, which has no JWT.
fn assertion_is_recent(assertion: Option<&str>) -> bool {
    let Some(assertion) = assertion else {
        return true;
    };
    use base64::Engine;
    let iat = assertion
        .split('.')
        .nth(1)
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload.trim_end_matches('='))
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|claims| claims.get("iat").and_then(serde_json::Value::as_i64));
    let now = chrono::Utc::now().timestamp();
    // A little clock skew either way; a missing `iat` is not recent.
    iat.is_some_and(|iat| iat <= now + 60 && now - iat <= IAP_MAX_AGE_SECONDS)
}

/// `GET /cli-auth`: ask the person to confirm.
pub async fn confirm(app: &App, browser: &Browser, query: Option<&str>) -> Reply {
    let Some(tokens) = app.person_tokens.as_ref().filter(|_| enabled(app)) else {
        return Reply::error(404, "not found");
    };
    let viewer = match viewer(app, browser.assertion.as_deref()).await {
        Ok(viewer) => viewer,
        Err(reply) => return reply,
    };
    let request = match parse_query(query)
        .ok_or("The query is not valid.")
        .and_then(|pairs| parse_request(&pairs))
    {
        Ok(request) => request,
        Err(message) => return page(400, "Bad request", message),
    };
    let expiry = (chrono::Utc::now().timestamp() + FORM_SECONDS).to_string();
    let port = request.port.to_string();
    let mac = tokens.mac(&mac_fields(&viewer.email, &request, &port, &expiry));
    let hidden = |name: &str, value: &str| {
        format!(
            "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
            escape(value)
        )
    };
    let place = match request.mode {
        Mode::Loopback => format!("on this computer (port {port})"),
        Mode::Copy => "on another computer, through a code you copy back to it".to_string(),
    };
    let detail = |label: &str, value: Option<String>| {
        value.map_or_else(String::new, |value| {
            format!("<li>{label}: <code>{}</code></li>", escape(&value))
        })
    };
    let origin_details = format!(
        "<ul>{}{}{}<li>Request: <code>{}</code></li></ul>",
        detail("From address", Some(browser.client_ip.to_string())),
        detail("Browser", short_user_agent(browser.user_agent.as_deref())),
        detail("Command run on", request.host.clone()),
        escape(request_id(&request.state)),
    );
    let mut fields = vec![
        hidden("user_code", &request.user_code),
        hidden("challenge", &request.challenge),
        hidden("state", &request.state),
        hidden("exp", &expiry),
        hidden("mac", &mac),
    ];
    if request.mode == Mode::Copy {
        fields.push(hidden("mode", "copy"));
    } else {
        fields.push(hidden("port", &port));
    }
    let body = format!(
        "<p>Confirm only if your terminal shows this code:</p>\
         <p style=\"font-size:28px;letter-spacing:.12em;font-family:monospace\"><strong>{user_code}</strong></p>\
         <p>Authorize the <code>aster</code> command {place} \
         to read the shared build cache as <strong>{email}</strong>?</p>\
         {origin_details}\
         <p class=\"muted\">It can read cache entries only, and the sign-in ends after a \
         few hours. If you did not just run <code>aster cache login</code>, or these details \
         are not yours, close this tab.</p>\
         <form method=\"post\" action=\"/cli-auth\">{fields}\
         <button type=\"submit\">Authorize</button></form>",
        user_code = escape(&display_user_code(&request.user_code)),
        email = escape(&viewer.email),
        fields = fields.concat(),
    );
    Reply::html(200, bare_page("Authorize aster CLI", &body))
}

/// `POST /cli-auth`: the person confirmed. Mint a code, then either send
/// the browser back to the CLI's loopback port or show the code.
pub async fn authorize(app: &App, browser: &Browser, body: &[u8]) -> Reply {
    let Some(tokens) = app.person_tokens.as_ref().filter(|_| enabled(app)) else {
        return Reply::error(404, "not found");
    };
    let viewer = match viewer(app, browser.assertion.as_deref()).await {
        Ok(viewer) => viewer,
        Err(reply) => return reply,
    };
    let expired = || {
        page(
            400,
            "Bad request",
            "This confirmation is not valid or has expired. Run `aster cache login` again.",
        )
    };
    if !app
        .config
        .public_url
        .as_deref()
        .is_some_and(|public| from_own_origin(public, browser))
    {
        return page(
            403,
            "Not allowed",
            "This confirmation was not submitted from this server's own page.",
        );
    }
    let Some(pairs) = std::str::from_utf8(body)
        .ok()
        .and_then(|text| parse_query(Some(text)))
    else {
        return expired();
    };
    let request = match parse_request(&pairs) {
        Ok(request) => request,
        Err(message) => return page(400, "Bad request", message),
    };
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map_or("", |(_, value)| value.as_str())
    };
    let (expiry, mac) = (field("exp"), field("mac"));
    let port = request.port.to_string();
    let fresh = expiry
        .parse::<i64>()
        .is_ok_and(|at| at > chrono::Utc::now().timestamp());
    if !fresh || !tokens.check_mac(&mac_fields(&viewer.email, &request, &port, expiry), mac) {
        audit::auth_failure(
            Some(&viewer.subject),
            "bad_confirmation",
            "the confirmation form's MAC or expiry is not valid",
        );
        return expired();
    }
    let code = match mint_code(app, &viewer, &request).await {
        Ok(code) => code,
        Err(error) => return unavailable(error),
    };
    audit::token_mint(&viewer.subject, &viewer.email, request.port);
    match request.mode {
        Mode::Loopback => Reply::redirect(format!(
            "http://127.0.0.1:{}/callback?code={code}&state={}",
            request.port, request.state
        )),
        Mode::Copy => Reply::html(
            200,
            bare_page(
                "Your sign-in code",
                &format!(
                    "<p>Paste this code into the terminal where you ran \
                     <code>aster cache login</code> (request <code>{id}</code>):</p>\
                     <pre><code>{code}</code></pre>\
                     <p class=\"muted\">It works once and expires in {CODE_SECONDS} seconds. \
                     It is not your token and gives nothing without that terminal.</p>",
                    id = escape(request_id(&request.state)),
                    code = escape(&code),
                ),
            ),
        ),
    }
}

async fn mint_code(app: &App, viewer: &Viewer, request: &Request) -> anyhow::Result<String> {
    let code = random_token(32)?;
    let client = app.pool.get().await?;
    purge_expired_with(&client).await?;
    client
        .execute(
            "INSERT INTO auth.cli_codes \
                 (code_hash, email, subject, challenge, port, user_code, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $7, now() + make_interval(secs => $6::double precision))",
            &[
                &hash(&code).as_slice(),
                &viewer.email,
                &viewer.subject,
                &request.challenge,
                &(request.port as i32),
                &(CODE_SECONDS as f64),
                &request.user_code,
            ],
        )
        .await?;
    Ok(code)
}

/// Drop codes nobody redeemed in time.
pub async fn purge_expired(app: &App) -> anyhow::Result<u64> {
    let client = app.pool.get().await?;
    purge_expired_with(&client).await
}

async fn purge_expired_with(client: &deadpool_postgres::Client) -> anyhow::Result<u64> {
    Ok(client
        .execute("DELETE FROM auth.cli_codes WHERE expires_at <= now()", &[])
        .await?)
}

fn hash(code: &str) -> [u8; 32] {
    Sha256::digest(code.as_bytes()).into()
}

fn unavailable(error: anyhow::Error) -> Reply {
    crate::log::error("cli sign-in failed", json!({"error": format!("{error:#}")}));
    Reply::error(500, "sign-in is unavailable")
}

#[derive(Deserialize)]
struct Exchange {
    code: String,
    verifier: String,
    /// What the person confirmed against their terminal.
    user_code: String,
    /// The loopback port; absent for a copy-mode code, which stores 0.
    #[serde(default)]
    port: u16,
}

fn invalid_grant() -> Reply {
    Reply::json(400, json!({"error": "invalid_grant"}))
}

/// The client sees one `invalid_grant`; the audit log and the failure
/// counter get the real reason.
fn refuse(subject: Option<&str>, reason: &'static str) -> Reply {
    audit::exchange_failed(subject, reason);
    invalid_grant()
}

/// Whether `text` is `len` base64url characters, the shape of a code
/// (`random_token(32)`) and of a PKCE challenge.
fn is_base64url_of(text: &str, len: usize) -> bool {
    text.len() == len && url_safe(text)
}

/// `POST /v1/auth/token`: trade a code and its PKCE verifier for a token.
///
/// The route is unauthenticated and reaches the database, so everything that
/// can be refused from the request alone is refused before a connection is
/// taken, and the query runs on the small [`App::auth_pool`].
pub async fn exchange(app: &App, body: &[u8]) -> Reply {
    let Some(tokens) = app.person_tokens.as_ref().filter(|_| enabled(app)) else {
        return Reply::error(404, "not found");
    };
    let Ok(request) = serde_json::from_slice::<Exchange>(body) else {
        return refuse(None, "malformed_request");
    };
    // RFC 7636: a verifier is 43 to 128 unreserved characters. A code is
    // exactly what `mint_code` makes, so any other shape cannot be one.
    let verifier_ok = (43..=128).contains(&request.verifier.len())
        && request
            .verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    // Port 0 is a copy-mode exchange; a loopback one names a real port.
    let port_ok = request.port == 0 || request.port >= MIN_PORT;
    if !verifier_ok || !is_base64url_of(&request.code, 43) || !port_ok {
        return refuse(None, "malformed_request");
    }
    let row = async {
        let client = app.auth_pool.get().await?;
        // One statement reads and spends the code, so of two racing
        // redemptions, on any replicas, exactly one gets the row.
        client
            .query_opt(
                "DELETE FROM auth.cli_codes WHERE code_hash = $1 AND expires_at > now() \
                 RETURNING email, subject, challenge, port, user_code, \
                 floor(extract(epoch FROM now()))::bigint",
                &[&hash(&request.code).as_slice()],
            )
            .await
            .map_err(anyhow::Error::from)
    }
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return refuse(None, "unknown_or_expired_code"),
        Err(error) => return unavailable(error),
    };
    // The row is spent whatever follows: an intercepted code can burn a
    // login, never complete one; that is what single use means.
    let (email, subject, challenge, port): (String, Option<String>, String, i32) =
        (row.get(0), row.get(1), row.get(2), row.get(3));
    let user_code: Option<String> = row.get(4);
    // Issue on the database clock, which logout cutoffs also use.
    let issued_at: i64 = row.get(5);
    let Some(subject) = subject.filter(|subject| !subject.is_empty()) else {
        return refuse(None, "code_without_subject");
    };
    let derived = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(request.verifier.as_bytes()))
    };
    // Constant time: the challenge is the credential's other half.
    if !crate::auth::constant_time_eq(derived.as_bytes(), challenge.as_bytes()) {
        return refuse(Some(&subject), "pkce_mismatch");
    }
    // What the person confirmed against their terminal. A row from before
    // the column has none and cannot be redeemed.
    let user_code_ok = user_code.is_some_and(|stored| {
        crate::auth::constant_time_eq(stored.as_bytes(), request.user_code.as_bytes())
    });
    if !user_code_ok {
        return refuse(Some(&subject), "user_code_mismatch");
    }
    // The port is the mode: a loopback code needs its port, a copy code none.
    if port != i32::from(request.port) {
        return refuse(Some(&subject), "port_mismatch");
    }
    match tokens.issue_at(&subject, &email, issued_at) {
        Ok(issued) => {
            app.metrics.person_token_issued();
            audit::exchange_succeeded(&subject);
            Reply::json(
                200,
                json!({
                    "token": issued.token,
                    "expires_at": issued.expires_at,
                    "email": email,
                }),
            )
        }
        Err(error) => unavailable(error),
    }
}

/// `POST /v1/auth/revoke`: refuse every token issued to `person` up to now,
/// this one included. The route table's `Person` class has already checked
/// the bearer token (and the kill switch), so no IAP session is involved.
pub async fn revoke(app: &App, person: &crate::person::Person) -> Reply {
    match crate::person::revoke_all(&app.pool, &person.subject).await {
        Ok(()) => {
            audit::token_revoke(&person.subject, &person.subject);
            Reply::json(200, json!({"revoked": true}))
        }
        Err(error) => unavailable(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with(payload: &str) -> String {
        use base64::Engine;
        let part = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        format!("{}.{}.sig", part("{}"), part(payload))
    }

    #[test]
    fn an_assertion_is_recent_for_five_minutes_and_needs_an_iat() {
        let now = chrono::Utc::now().timestamp();
        let at = |iat: i64| jwt_with(&format!("{{\"iat\":{iat}}}"));
        assert!(assertion_is_recent(Some(&at(now - 10))));
        assert!(assertion_is_recent(Some(&at(now - 299))));
        assert!(!assertion_is_recent(Some(&at(now - 301))));
        assert!(!assertion_is_recent(Some(&at(now + 120))));
        assert!(!assertion_is_recent(Some(&jwt_with("{}"))));
        assert!(!assertion_is_recent(Some(&jwt_with("{\"iat\":\"x\"}"))));
        assert!(!assertion_is_recent(Some("not-a-jwt")));
        // The development viewer has no assertion at all.
        assert!(assertion_is_recent(None));
    }

    #[test]
    fn only_the_servers_own_origin_may_submit() {
        let browser = |origin: Option<&str>, referer: Option<&str>| Browser {
            assertion: None,
            origin: origin.map(str::to_string),
            referer: referer.map(str::to_string),
            client_ip: std::net::Ipv4Addr::LOCALHOST.into(),
            user_agent: None,
        };
        let own = "https://aster-srv.example.com";
        assert!(from_own_origin(own, &browser(Some(own), None)));
        assert!(from_own_origin(
            "https://aster-srv.example.com/base",
            &browser(Some(own), None)
        ));
        assert!(from_own_origin(
            own,
            &browser(None, Some("https://aster-srv.example.com/cli-auth?x=1"))
        ));
        assert!(!from_own_origin(own, &browser(None, None)));
        assert!(!from_own_origin(own, &browser(Some("null"), None)));
        assert!(!from_own_origin(
            own,
            &browser(Some("https://aster-srv.example.com.evil.io"), None)
        ));
        assert!(!from_own_origin(
            own,
            &browser(None, Some("https://aster-srv.example.com.evil.io/"))
        ));
        assert!(!from_own_origin(
            own,
            &browser(Some("http://aster-srv.example.com"), None)
        ));
        assert!(!from_own_origin("not a url", &browser(Some(own), None)));
    }
}
