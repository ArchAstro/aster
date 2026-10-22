//! Sign-in for `aster cache login`: an OAuth-style authorization-code flow
//! with PKCE, run against the laptop's loopback port.
//!
//! ```text
//! aster                       browser (behind IAP)                aster-srv
//!   | listen 127.0.0.1:PORT          |                                |
//!   | open /cli-auth?port&challenge&state ----------------------->   | GET: IAP email, confirm page + MAC
//!   |                                | POST /cli-auth (form) ------>  | check IAP, MAC; store hash(code)
//!   |  <-- 302 http://127.0.0.1:PORT/callback?code&state ----------   |
//!   | POST /v1/auth/token {code, verifier, port} ------------------>  | DELETE code row, check PKCE + port
//!   |  <-- {token, expires_at} --------------------------------------|
//! ```
//!
//! `/cli-auth` sits behind IAP, which is how the person is known. `/v1/auth/token`
//! does not: the CLI has no IAP session, and the code plus PKCE verifier are
//! its credential. A code is 256 random bits, kept only as its SHA-256, valid
//! for 60 seconds and spent by the `DELETE` that reads it, so it works once
//! even with several replicas. Every refusal at exchange is the same
//! `400 invalid_grant`.
//!
//! The redirect goes to `http://127.0.0.1:<port>/callback` and nowhere else.
//! The confirmation page asks before redirecting, and its form carries a MAC
//! over the person, challenge, port, state and expiry, so a page for one
//! person cannot be submitted for another (login CSRF).

use hyper::Method;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::db::Migration;
use crate::iap::{Viewer, ViewerError};
use crate::person::random_token;
use crate::test_results::query::parse_query;
use crate::ui::html::{bare_page, escape};
use crate::{App, Reply};

pub const MIGRATIONS: &[Migration] = &[Migration {
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
}];

/// How long a code can be redeemed.
const CODE_SECONDS: i64 = 60;
/// How long the confirmation page's form is good for.
const FORM_SECONDS: i64 = 600;
/// The largest form body.
pub const MAX_FORM_BYTES: usize = 8 * 1024;
const MIN_PORT: u16 = 1024;

/// `GET|POST /cli-auth` or `POST /v1/auth/token`.
pub fn is_route(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (&Method::GET | &Method::POST, "/cli-auth") | (&Method::POST, "/v1/auth/token")
    )
}

/// What the CLI asks for.
struct Request {
    port: u16,
    challenge: String,
    state: String,
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
    let port = find("port");
    let port = (!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        .then(|| port.parse::<u16>().ok())
        .flatten()
        .filter(|port| *port >= MIN_PORT)
        .ok_or("The port must be a number from 1024 to 65535.")?;
    let challenge = find("challenge");
    // A SHA-256 as unpadded base64url.
    if challenge.len() != 43 || !url_safe(challenge) {
        return Err("The challenge must be a 43-character base64url SHA-256.");
    }
    let state = find("state");
    if state.is_empty() || state.len() > 128 || !url_safe(state) {
        return Err("The state must be 1 to 128 base64url characters.");
    }
    Ok(Request {
        port,
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

/// Whether sign-in is on: the keys and public URL are set and IAP (or the
/// development viewer) can name a person.
fn enabled(app: &App) -> bool {
    app.person_tokens.is_some() && app.iap.enabled()
}

async fn viewer(app: &App, assertion: Option<&str>) -> Result<Viewer, Reply> {
    match app.iap.verify(assertion).await {
        Ok(viewer) if !viewer.email.contains('\0') => Ok(viewer),
        Ok(_) => Err(page(401, "Sign in", "The sign-in assertion is not valid.")),
        Err(error @ ViewerError::Forbidden(_)) => Err(page(403, "Not allowed", &error.to_string())),
        Err(error) => Err(page(401, "Sign in", &error.to_string())),
    }
}

/// `GET /cli-auth`: ask the person to confirm.
pub async fn confirm(app: &App, assertion: Option<&str>, query: Option<&str>) -> Reply {
    let Some(tokens) = app.person_tokens.as_ref().filter(|_| enabled(app)) else {
        return Reply::error(404, "not found");
    };
    let viewer = match viewer(app, assertion).await {
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
    let mac = tokens.mac(&[
        &viewer.email,
        &request.challenge,
        &port,
        &request.state,
        &expiry,
    ]);
    let hidden = |name: &str, value: &str| {
        format!(
            "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
            escape(value)
        )
    };
    let body = format!(
        "<p>Authorize the <code>aster</code> command on this computer (port {port}) \
         to read the shared build cache as <strong>{email}</strong>?</p>\
         <p class=\"muted\">It can read cache entries only, and the sign-in ends after a \
         few hours. If you did not just run <code>aster cache login</code>, close this tab.</p>\
         <form method=\"post\" action=\"/cli-auth\">{fields}\
         <button type=\"submit\">Authorize</button></form>",
        email = escape(&viewer.email),
        fields = [
            hidden("port", &port),
            hidden("challenge", &request.challenge),
            hidden("state", &request.state),
            hidden("exp", &expiry),
            hidden("mac", &mac),
        ]
        .concat(),
    );
    Reply::html(200, bare_page("Authorize aster CLI", &body))
}

/// `POST /cli-auth`: the person confirmed. Mint a code and send the browser
/// back to the CLI's loopback port.
pub async fn authorize(app: &App, assertion: Option<&str>, body: &[u8]) -> Reply {
    let Some(tokens) = app.person_tokens.as_ref().filter(|_| enabled(app)) else {
        return Reply::error(404, "not found");
    };
    let viewer = match viewer(app, assertion).await {
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
    if !fresh
        || !tokens.check_mac(
            &[
                &viewer.email,
                &request.challenge,
                &port,
                &request.state,
                expiry,
            ],
            mac,
        )
    {
        return expired();
    }
    let code = match mint_code(app, &viewer.email, &request).await {
        Ok(code) => code,
        Err(error) => return unavailable(error),
    };
    Reply::redirect(format!(
        "http://127.0.0.1:{}/callback?code={code}&state={}",
        request.port, request.state
    ))
}

async fn mint_code(app: &App, email: &str, request: &Request) -> anyhow::Result<String> {
    let code = random_token(32)?;
    let client = app.pool.get().await?;
    purge_expired_with(&client).await?;
    client
        .execute(
            "INSERT INTO auth.cli_codes (code_hash, email, challenge, port, expires_at) \
             VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5::double precision))",
            &[
                &hash(&code).as_slice(),
                &email,
                &request.challenge,
                &(request.port as i32),
                &(CODE_SECONDS as f64),
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
    port: u16,
}

fn invalid_grant() -> Reply {
    Reply::json(400, json!({"error": "invalid_grant"}))
}

/// `POST /v1/auth/token`: trade a code and its PKCE verifier for a token.
pub async fn exchange(app: &App, body: &[u8]) -> Reply {
    let Some(tokens) = app.person_tokens.as_ref().filter(|_| enabled(app)) else {
        return Reply::error(404, "not found");
    };
    let Ok(request) = serde_json::from_slice::<Exchange>(body) else {
        return invalid_grant();
    };
    // RFC 7636: a verifier is 43 to 128 unreserved characters.
    let verifier_ok = (43..=128).contains(&request.verifier.len())
        && request
            .verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    if !verifier_ok || request.code.len() > 128 {
        return invalid_grant();
    }
    let row = async {
        let client = app.pool.get().await?;
        // One statement reads and spends the code, so of two racing
        // redemptions, on any replicas, exactly one gets the row.
        client
            .query_opt(
                "DELETE FROM auth.cli_codes WHERE code_hash = $1 AND expires_at > now() \
                 RETURNING email, challenge, port",
                &[&hash(&request.code).as_slice()],
            )
            .await
            .map_err(anyhow::Error::from)
    }
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return invalid_grant(),
        Err(error) => return unavailable(error),
    };
    let (email, challenge, port): (String, String, i32) = (row.get(0), row.get(1), row.get(2));
    let derived = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(request.verifier.as_bytes()))
    };
    let verified = crate::auth::constant_time_eq(derived.as_bytes(), challenge.as_bytes());
    if !verified || port != i32::from(request.port) {
        return invalid_grant();
    }
    match tokens.issue(&email) {
        Ok(issued) => Reply::json(
            200,
            json!({
                "token": issued.token,
                "expires_at": issued.expires_at,
                "email": email,
            }),
        ),
        Err(error) => unavailable(error),
    }
}
