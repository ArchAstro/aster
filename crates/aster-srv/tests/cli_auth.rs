//! Laptop sign-in (`/cli-auth`, `/v1/auth/token`) and what a person's token
//! may do, against real Postgres.
//!
//! The browser is a scripted HTTP client that attaches a signed IAP
//! assertion, as IAP would. Set `ASTER_SRV_TEST_DATABASE_URL`; without it the
//! tests skip.

mod common;

use std::sync::Arc;

use aster_srv::cache::{BlobStore, FixedSigner, Signer};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use common::{iap_assertion, iap_assertion_for, person_bearer, token_with_claims, Server};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const REPO: &str = "ArchAstro/firstlanding";
const ADA: &str = "ada@example.com";
const CI_WORKFLOW: &str = "ArchAstro/firstlanding/.github/workflows/ci.yml@refs/heads/main";

fn server_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
        (
            "ASTER_SRV_CACHE_SIGNER",
            "cache-test@example.iam.gserviceaccount.com",
        ),
        (
            "ASTER_SRV_IAP_AUDIENCE",
            "/projects/123456/global/backendServices/",
        ),
        ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
    ]
}

async fn start_with(extra: &[(&str, &str)]) -> Option<Server> {
    let signer = Arc::new(FixedSigner {
        email: "cache-test@example.iam.gserviceaccount.com".into(),
        signature: b"test-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory("aster-cache-test", signer as Arc<dyn Signer>);
    let mut env = server_env();
    env.extend_from_slice(extra);
    // Later entries win: `find` takes the first, so put the extras first.
    env.reverse();
    common::start_with(&env, Some(store)).await
}

async fn start() -> Option<Server> {
    start_with(&[]).await
}

/// A PKCE verifier and its S256 challenge.
fn pkce() -> (String, String) {
    let verifier = BASE64URL.encode(rand_bytes());
    let challenge = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn rand_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    bytes
}

fn browser() -> reqwest::Client {
    aster_srv::http_client()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// What a browser got back.
struct Page {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

impl Page {
    fn location(&self) -> Option<&str> {
        self.headers.get("location").and_then(|v| v.to_str().ok())
    }
}

/// `GET /cli-auth` as a browser behind IAP (`assertion` is the IAP header).
async fn open_login(server: &Server, query: &str, assertion: Option<&str>) -> Page {
    let mut request = browser().get(format!("{}/cli-auth?{query}", server.url));
    if let Some(assertion) = assertion {
        request = request.header("x-goog-iap-jwt-assertion", assertion);
    }
    page(request.send().await.unwrap()).await
}

/// Submit the confirmation form.
async fn confirm(server: &Server, fields: &[(&str, &str)], assertion: Option<&str>) -> Page {
    let body = fields
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut request = browser()
        .post(format!("{}/cli-auth", server.url))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body);
    if let Some(assertion) = assertion {
        request = request.header("x-goog-iap-jwt-assertion", assertion);
    }
    page(request.send().await.unwrap()).await
}

async fn page(response: reqwest::Response) -> Page {
    Page {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        body: response.text().await.unwrap_or_default(),
    }
}

/// The hidden fields of the confirmation form, in page order.
fn form_fields(html: &str) -> Vec<(String, String)> {
    html.split("<input type=\"hidden\" name=\"")
        .skip(1)
        .map(|part| {
            let (name, rest) = part.split_once("\" value=\"").unwrap();
            let (value, _) = rest.split_once('"').unwrap();
            (name.to_string(), value.to_string())
        })
        .collect()
}

fn as_refs(fields: &[(String, String)]) -> Vec<(&str, &str)> {
    fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

fn query(port: impl std::fmt::Display, challenge: &str, state: &str) -> String {
    format!("port={port}&challenge={challenge}&state={state}")
}

/// The code and state a successful confirmation sent to the loopback port.
fn callback(location: &str, port: u16) -> (String, String) {
    let rest = location
        .strip_prefix(&format!("http://127.0.0.1:{port}/callback?code="))
        .unwrap_or_else(|| panic!("redirect must be loopback only: {location}"));
    let (code, state) = rest.split_once("&state=").unwrap();
    (code.to_string(), state.to_string())
}

/// Sign in as `email` and return what the CLI would hold: the code, verifier and port.
async fn sign_in(server: &Server, email: &str) -> (String, String, u16) {
    let (verifier, challenge) = pkce();
    let port = 40_123;
    let assertion = iap_assertion(email);
    let form = open_login(
        server,
        &query(port, &challenge, "state-1"),
        Some(&assertion),
    )
    .await;
    assert_eq!(form.status, 200, "{}", form.body);
    let fields = form_fields(&form.body);
    let done = confirm(server, &as_refs(&fields), Some(&assertion)).await;
    assert_eq!(done.status, 302, "{}", done.body);
    let (code, state) = callback(done.location().unwrap(), port);
    assert_eq!(state, "state-1");
    (code, verifier, port)
}

async fn exchange(server: &Server, code: &str, verifier: &str, port: u16) -> (u16, Value) {
    server
        .post_json(
            "/v1/auth/token",
            &json!({"code": code, "verifier": verifier, "port": port}),
            None,
            None,
        )
        .await
}

fn invalid_grant() -> Value {
    json!({"error": "invalid_grant"})
}

#[tokio::test]
async fn sign_in_needs_an_iap_assertion_from_an_allowed_domain() {
    let Some(server) = start().await else {
        return;
    };
    let (_, challenge) = pkce();
    let q = query(40_000, &challenge, "s");
    let wrong_audience = iap_assertion_for(ADA, "/projects/999/global/backendServices/1");
    let outsider = iap_assertion("eve@other.test");
    let forged = {
        // Signed with a key IAP never published.
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("iap".into());
        encode(
            &header,
            &json!({"iss": "https://cloud.google.com/iap", "aud": "/projects/123456/global/backendServices/4242",
                    "exp": chrono::Utc::now().timestamp() + 300, "email": ADA}),
            &EncodingKey::from_secret(b"not-the-iap-key"),
        )
        .unwrap()
    };
    for (what, assertion, status) in [
        ("missing", None, 401),
        ("garbage", Some("not-a-jwt".to_string()), 401),
        ("forged", Some(forged), 401),
        ("wrong audience", Some(wrong_audience), 401),
        ("wrong domain", Some(outsider), 403),
    ] {
        let got = open_login(&server, &q, assertion.as_deref()).await;
        assert_eq!(got.status, status, "GET with {what} assertion");
        let got = confirm(&server, &[("port", "40000")], assertion.as_deref()).await;
        assert_eq!(got.status, status, "POST with {what} assertion");
    }

    // A real one gets the confirmation page for the person IAP named.
    let ok = open_login(&server, &q, Some(&iap_assertion(ADA))).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert!(ok.body.contains("Authorize"), "{}", ok.body);
    assert!(ok.body.contains(ADA), "{}", ok.body);
    // The page is not framed or cached, and the CSP does not stop the form
    // submit's redirect to the loopback port.
    let csp = ok.headers["content-security-policy"].to_str().unwrap();
    assert!(!csp.contains("form-action"), "{csp}");
    assert_eq!(ok.headers["x-frame-options"], "DENY");
    assert_eq!(ok.headers["cache-control"], "no-store");
    server.stop().await;
}

#[tokio::test]
async fn sign_in_refuses_a_bad_port_challenge_or_state() {
    let Some(server) = start().await else {
        return;
    };
    let assertion = iap_assertion(ADA);
    let (_, challenge) = pkce();
    let long_state = "s".repeat(129);
    for (what, q) in [
        ("no port", format!("challenge={challenge}&state=s")),
        ("port below 1024", query(1023, &challenge, "s")),
        ("port 0", query(0, &challenge, "s")),
        ("port too large", query(65_536, &challenge, "s")),
        (
            "port not a number",
            format!("port=80a&challenge={challenge}&state=s"),
        ),
        ("short challenge", query(40_000, "abc", "s")),
        (
            "padded challenge",
            query(40_000, &format!("{challenge}="), "s"),
        ),
        (
            "challenge with a plus",
            query(40_000, &format!("{}+", &challenge[..42]), "s"),
        ),
        ("no state", format!("port=40000&challenge={challenge}")),
        ("empty state", query(40_000, &challenge, "")),
        ("long state", query(40_000, &challenge, &long_state)),
        ("state with markup", query(40_000, &challenge, "%3Cb%3E")),
    ] {
        let got = open_login(&server, &q, Some(&assertion)).await;
        assert_eq!(got.status, 400, "{what}: {}", got.body);
    }
    // Boundary values are fine.
    for port in [1024, 65_535] {
        let got = open_login(&server, &query(port, &challenge, "s"), Some(&assertion)).await;
        assert_eq!(got.status, 200, "port {port}");
    }
    server.stop().await;
}

#[tokio::test]
async fn the_confirmation_form_is_bound_to_who_asked_what_and_when() {
    let Some(server) = start().await else {
        return;
    };
    let ada = iap_assertion(ADA);
    let (_, challenge) = pkce();
    let q = query(40_000, &challenge, "state-x");
    let form = open_login(&server, &q, Some(&ada)).await;
    let fields = form_fields(&form.body);
    let names: Vec<_> = fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["port", "challenge", "state", "exp", "mac"]);

    let with = |name: &str, value: &str| -> Vec<(String, String)> {
        fields
            .iter()
            .map(|(n, v)| (n.clone(), if n == name { value.into() } else { v.clone() }))
            .collect()
    };
    let other_challenge = pkce().1;
    let mut attempts: Vec<(&str, Vec<(String, String)>)> = vec![
        ("tampered port", with("port", "40001")),
        ("tampered challenge", with("challenge", &other_challenge)),
        ("tampered state", with("state", "state-y")),
        ("tampered expiry", with("exp", "99999999999")),
        ("tampered mac", with("mac", &BASE64URL.encode([7u8; 32]))),
        ("garbage mac", with("mac", "!!")),
        ("empty mac", with("mac", "")),
        (
            "no mac",
            fields.iter().filter(|(n, _)| n != "mac").cloned().collect(),
        ),
    ];
    // A form rendered for someone else (login CSRF): Bob's page, Ada's browser.
    let bob_form = open_login(&server, &q, Some(&iap_assertion("bob@example.com"))).await;
    attempts.push((
        "a form rendered for another person",
        form_fields(&bob_form.body),
    ));
    // A correctly MACed form whose time has passed.
    let issuer = server.app.person_tokens.as_ref().unwrap();
    let past = (chrono::Utc::now().timestamp() - 5).to_string();
    let mac = issuer.mac(&[ADA, &challenge, "40000", "state-x", &past]);
    attempts.push((
        "an expired form",
        vec![
            ("port".into(), "40000".into()),
            ("challenge".into(), challenge.clone()),
            ("state".into(), "state-x".into()),
            ("exp".into(), past),
            ("mac".into(), mac),
        ],
    ));
    for (what, fields) in &attempts {
        let got = confirm(&server, &as_refs(fields), Some(&ada)).await;
        assert_eq!(got.status, 400, "{what}: {}", got.body);
        assert!(got.location().is_none(), "{what} must not redirect");
    }
    let minted = server
        .client()
        .await
        .query_one("SELECT count(*) FROM auth.cli_codes", &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(minted, 0, "no refused form may mint a code");

    // The genuine form works, and redirects to the loopback port only.
    let done = confirm(&server, &as_refs(&fields), Some(&ada)).await;
    assert_eq!(done.status, 302, "{}", done.body);
    let (code, state) = callback(done.location().unwrap(), 40_000);
    assert_eq!(state, "state-x");
    assert_eq!(code.len(), 43, "a 256-bit code, base64url");
    assert_eq!(done.headers["cache-control"], "no-store");
    server.stop().await;
}

#[tokio::test]
async fn a_code_redeems_once_for_a_token_that_names_the_person() {
    let Some(server) = start().await else {
        return;
    };
    let (code, verifier, port) = sign_in(&server, ADA).await;

    // Only the SHA-256 of the code is stored.
    let row = server
        .client()
        .await
        .query_one(
            "SELECT email, port, code_hash = sha256(convert_to($1, 'UTF8')) AS hashed \
             FROM auth.cli_codes",
            &[&code],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), ADA);
    assert_eq!(row.get::<_, i32>(1), i32::from(port));
    assert!(row.get::<_, bool>(2), "the row is keyed by the code's hash");

    let (status, reply) = exchange(&server, &code, &verifier, port).await;
    assert_eq!(status, 200, "{reply}");
    let token = reply["token"].as_str().unwrap();
    let expires = reply["expires_at"].as_i64().unwrap();
    let lifetime = expires - chrono::Utc::now().timestamp();
    assert!(
        (12 * 60 * 60 - 10..=12 * 60 * 60).contains(&lifetime),
        "{reply}"
    );
    assert_eq!(reply["email"], ADA);

    // The token reads the cache.
    let (status, hits) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&format!("Bearer {token}")),
            None,
        )
        .await;
    assert_eq!(status, 200, "{hits}");

    // Replay: the code is spent.
    let (status, reply) = exchange(&server, &code, &verifier, port).await;
    assert_eq!((status, reply), (400, invalid_grant()));
    server.stop().await;
}

#[tokio::test]
async fn every_refused_exchange_is_the_same_invalid_grant() {
    let Some(server) = start().await else {
        return;
    };
    let post = |body: Value| {
        let server = &server;
        async move { server.post_json("/v1/auth/token", &body, None, None).await }
    };

    // Wrong verifier (and the code is burned for the right one afterwards).
    let (code, verifier, port) = sign_in(&server, ADA).await;
    let (_, other_verifier) = pkce();
    assert_eq!(
        post(json!({"code": code, "verifier": other_verifier, "port": port})).await,
        (400, invalid_grant())
    );
    assert_eq!(
        exchange(&server, &code, &verifier, port).await,
        (400, invalid_grant())
    );

    // Wrong port.
    let (code, verifier, port) = sign_in(&server, ADA).await;
    assert_eq!(
        exchange(&server, &code, &verifier, port + 1).await,
        (400, invalid_grant())
    );

    // Expired.
    let (code, verifier, port) = sign_in(&server, ADA).await;
    server
        .client()
        .await
        .execute(
            "UPDATE auth.cli_codes SET expires_at = now() - interval '1 second'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        exchange(&server, &code, &verifier, port).await,
        (400, invalid_grant())
    );

    // Never issued, malformed, short verifier, wrong types.
    let (_, verifier) = pkce();
    for body in [
        json!({"code": "nope", "verifier": verifier, "port": 40_123}),
        json!({"code": "", "verifier": "", "port": 40_123}),
        json!({"code": "x", "verifier": "short", "port": 40_123}),
        json!({"code": 1, "verifier": verifier, "port": 40_123}),
        json!({"code": "x", "verifier": verifier}),
        json!({}),
    ] {
        assert_eq!(post(body.clone()).await, (400, invalid_grant()), "{body}");
    }
    let raw = browser()
        .post(format!("{}/v1/auth/token", server.url))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status().as_u16(), 400);
    assert_eq!(raw.json::<Value>().await.unwrap(), invalid_grant());
    server.stop().await;
}

#[tokio::test]
async fn a_code_minted_on_one_server_redeems_on_another() {
    let Some(a) = start().await else {
        return;
    };
    let b = a.second(&[]).await;
    let (code, verifier, port) = sign_in(&a, ADA).await;
    // Server B shares only Postgres with A, and also has the same signing keys.
    let (status, reply) = exchange(&b, &code, &verifier, port).await;
    assert_eq!(status, 200, "{reply}");
    // A cannot redeem it again: B spent it.
    assert_eq!(
        exchange(&a, &code, &verifier, port).await,
        (400, invalid_grant())
    );
    b.stop().await;
    a.stop().await;
}

#[tokio::test]
async fn two_racing_redemptions_of_one_code_have_exactly_one_winner() {
    let Some(a) = start().await else {
        return;
    };
    let b = Arc::new(a.second(&[]).await);
    let a = Arc::new(a);
    let (code, verifier, port) = sign_in(&a, ADA).await;
    let mut racers = tokio::task::JoinSet::new();
    for index in 0..16 {
        let server = if index % 2 == 0 { a.clone() } else { b.clone() };
        let (code, verifier) = (code.clone(), verifier.clone());
        racers.spawn(async move { exchange(&server, &code, &verifier, port).await.0 });
    }
    let mut statuses = Vec::new();
    while let Some(status) = racers.join_next().await {
        statuses.push(status.unwrap());
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 400).count(),
        15,
        "{statuses:?}"
    );
    Arc::into_inner(b).unwrap().stop().await;
    Arc::into_inner(a).unwrap().stop().await;
}

#[tokio::test]
async fn expired_codes_are_purged_when_a_code_is_minted_and_by_the_sweep() {
    let Some(server) = start().await else {
        return;
    };
    let insert_stale = |email: &'static str| {
        let server = &server;
        async move {
            server
                .client()
                .await
                .execute(
                    "INSERT INTO auth.cli_codes VALUES (sha256(convert_to($1, 'UTF8')), $1, 'c', 4000, now() - interval '1 minute')",
                    &[&email],
                )
                .await
                .unwrap();
        }
    };
    let rows = || async {
        server
            .client()
            .await
            .query_one("SELECT count(*) FROM auth.cli_codes", &[])
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    insert_stale("stale-1").await;
    assert_eq!(rows().await, 1);
    sign_in(&server, ADA).await;
    assert_eq!(
        rows().await,
        1,
        "minting purged the stale row, leaving the new one"
    );
    insert_stale("stale-2").await;
    assert_eq!(
        aster_srv::cli_auth::purge_expired(&server.app)
            .await
            .unwrap(),
        1
    );
    assert_eq!(rows().await, 1, "the sweep keeps live codes");
    server.stop().await;
}

#[tokio::test]
async fn a_person_reads_the_cache_and_nothing_else() {
    let Some(server) = start().await else {
        return;
    };
    let person = person_bearer(&server, ADA);
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let manifest = BASE64STD.encode(
        serde_json::to_vec(&json!({"schema": 1, "key": "k", "target": "//a:b", "outputs": []}))
            .unwrap(),
    );
    let get = |path: &'static str| {
        let (server, person) = (&server, &person);
        async move { server.get_json(path, Some(person), None).await.0 }
    };
    let post = |path: &'static str, body: Value| {
        let (server, person) = (&server, &person);
        async move { server.post_json(path, &body, Some(person), None).await.0 }
    };
    // Reads.
    assert_eq!(
        post("/v1/cache/lookup", json!({"repo": REPO, "keys": ["k"]})).await,
        200
    );
    assert_eq!(
        get("/v1/cache/summary?repo=ArchAstro/firstlanding").await,
        200
    );
    assert_eq!(
        get("/v1/cache/entries?repo=ArchAstro/firstlanding").await,
        200
    );
    // Writes, even though IAP's email domain and the repository are fine.
    assert_eq!(
        post(
            "/v1/cache/upload",
            json!({"repo": REPO, "digests": [digest]})
        )
        .await,
        403
    );
    assert_eq!(
        post(
            "/v1/cache/commit",
            json!({"repo": REPO, "key": "k", "manifest": manifest})
        )
        .await,
        403
    );
    // A repository this server does not serve.
    assert_eq!(
        post("/v1/cache/lookup", json!({"repo": "evil/x", "keys": ["k"]})).await,
        403
    );
    assert_eq!(get("/v1/cache/summary?repo=evil/x").await, 403);
    // Another domain's person, whose token this server issued earlier.
    let outsider = person_bearer(&server, "eve@other.test");
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&outsider),
            None,
        )
        .await;
    assert_eq!(status, 403);
    // Test results are CI's, not people's.
    for (method, path) in [
        ("POST", "/v1/test-results/runs"),
        ("GET", "/v1/test-results/durations?target=%2F%2Fa%3Ab"),
        ("GET", "/v1/test-results/tests"),
        ("GET", "/v1/test-results/quarantine"),
    ] {
        let client = browser();
        let url = format!("{}{path}", server.url);
        let request = if method == "POST" {
            client.post(url).body("{}")
        } else {
            client.get(url)
        };
        let response = request
            .header("authorization", &person)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 403, "{method} {path}");
    }
    server.stop().await;
}

use base64::engine::general_purpose::STANDARD as BASE64STD;

#[tokio::test]
async fn a_person_token_that_is_expired_forged_misdirected_or_rotated_out_is_refused() {
    let Some(server) = start().await else {
        return;
    };
    let secret = common::CLI_TOKEN_KEYS.split_once(':').unwrap().1;
    let claims = |change: &dyn Fn(&mut Value)| {
        let mut claims = json!({
            "iss": common::PUBLIC_URL, "aud": "aster-cli", "sub": ADA, "scope": "cache:read",
            "iat": chrono::Utc::now().timestamp(), "exp": chrono::Utc::now().timestamp() + 600,
            "jti": "j",
        });
        change(&mut claims);
        claims
    };
    let sign = |claims: Value, kid: &str, secret: &str| {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.into());
        format!(
            "Bearer {}",
            encode(
                &header,
                &claims,
                &EncodingKey::from_secret(secret.as_bytes())
            )
            .unwrap()
        )
    };
    let lookup = |bearer: String| {
        let server = &server;
        async move {
            server
                .post_json(
                    "/v1/cache/lookup",
                    &json!({"repo": REPO, "keys": ["k"]}),
                    Some(&bearer),
                    None,
                )
                .await
                .0
        }
    };
    assert_eq!(
        lookup(sign(claims(&|_| {}), "t1", secret)).await,
        200,
        "control"
    );
    for (what, bearer) in [
        (
            "expired",
            sign(
                claims(&|c| c["exp"] = json!(chrono::Utc::now().timestamp() - 3600)),
                "t1",
                secret,
            ),
        ),
        (
            "bad signature",
            sign(claims(&|_| {}), "t1", "another-secret-0123456789abcdefghij"),
        ),
        (
            "wrong audience",
            sign(claims(&|c| c["aud"] = json!("aster-srv")), "t1", secret),
        ),
        (
            "wrong issuer",
            sign(
                claims(&|c| c["iss"] = json!("https://evil.test")),
                "t1",
                secret,
            ),
        ),
        ("unknown kid", sign(claims(&|_| {}), "t0", secret)),
    ] {
        assert_eq!(lookup(bearer).await, 401, "{what}");
    }

    // Rotation: the same server restarted with the key dropped.
    let old = person_bearer(&server, ADA);
    let rotated = server
        .second(&[(
            "ASTER_SRV_CLI_TOKEN_KEYS",
            "t2:rotated-signing-secret-0123456789abcd",
        )])
        .await;
    let (status, _) = rotated
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&old),
            None,
        )
        .await;
    assert_eq!(status, 401, "a token signed by a rotated-out key");
    // During a rotation both keys verify.
    let both = server
        .second(&[(
            "ASTER_SRV_CLI_TOKEN_KEYS",
            "t2:rotated-signing-secret-0123456789abcd,t1:test-signing-secret-0123456789abcdef",
        )])
        .await;
    let (status, _) = both
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&old),
            None,
        )
        .await;
    assert_eq!(
        status, 200,
        "a token signed by the previous key during rotation"
    );
    both.stop().await;
    rotated.stop().await;
    server.stop().await;
}

#[tokio::test]
async fn a_v1_request_needs_its_own_bearer_token_and_iap_alone_is_not_one() {
    let Some(server) = start().await else {
        return;
    };
    let ada_iap = iap_assertion(ADA);
    let body = json!({"repo": REPO, "keys": ["k"]});
    let upload = json!({"repo": REPO, "digests": []});
    let commit = json!({"repo": REPO, "key": "k", "manifest": ""});
    let forged_iap = "eyJhbGciOiJFUzI1NiIsImtpZCI6ImlhcCJ9.e30.AAAA";
    for (what, bearer, iap) in [
        ("no credentials", None, None),
        (
            "a valid IAP assertion and no bearer",
            None,
            Some(ada_iap.as_str()),
        ),
        ("a forged IAP header", None, Some(forged_iap)),
        ("a garbage bearer", Some("Bearer not-a-token"), None),
        (
            "a garbage bearer and a valid IAP assertion",
            Some("Bearer nope"),
            Some(ada_iap.as_str()),
        ),
        ("a basic credential", Some("Basic YTpi"), None),
    ] {
        for (path, body) in [
            ("/v1/cache/lookup", &body),
            ("/v1/cache/upload", &upload),
            ("/v1/cache/commit", &commit),
        ] {
            let (status, _) = server.post_json(path, body, bearer, iap).await;
            assert_eq!(status, 401, "{what} on {path}");
        }
        for path in [
            "/v1/cache/summary?repo=ArchAstro/firstlanding",
            "/v1/cache/entries?repo=ArchAstro/firstlanding",
            "/v1/test-results/tests",
        ] {
            let (status, _) = server.get_json(path, bearer, iap).await;
            assert_eq!(status, 401, "{what} on {path}");
        }
    }
    // The dashboard is unchanged: IAP is its credential.
    let (status, _, _) = server
        .get_page("/ui/cache?repo=ArchAstro%2Ffirstlanding", Some(&ada_iap))
        .await;
    assert_eq!(status, 200);
    server.stop().await;
}

#[tokio::test]
async fn without_keys_or_a_public_url_sign_in_is_off_and_person_tokens_are_refused() {
    let Some(on) = start().await else {
        return;
    };
    let token = person_bearer(&on, ADA);
    let (_, challenge) = pkce();
    for (what, env) in [
        ("no keys", ("ASTER_SRV_CLI_TOKEN_KEYS", "")),
        ("no public url", ("ASTER_SRV_PUBLIC_URL", "")),
    ] {
        let off = on.second(&[env]).await;
        let got = open_login(
            &off,
            &query(40_000, &challenge, "s"),
            Some(&iap_assertion(ADA)),
        )
        .await;
        assert_eq!(got.status, 404, "{what}: GET /cli-auth");
        let got = confirm(&off, &[("port", "40000")], Some(&iap_assertion(ADA))).await;
        assert_eq!(got.status, 404, "{what}: POST /cli-auth");
        let (status, _) = exchange(&off, "x", &pkce().0, 40_000).await;
        assert_eq!(status, 404, "{what}: POST /v1/auth/token");
        let (status, _) = off
            .post_json(
                "/v1/cache/lookup",
                &json!({"repo": REPO, "keys": ["k"]}),
                Some(&token),
                None,
            )
            .await;
        assert_eq!(status, 401, "{what}: a person token is refused");
        off.stop().await;
    }
    // Without IAP configured there is nobody to name either.
    let no_iap = on.second(&[("ASTER_SRV_IAP_AUDIENCE", "")]).await;
    let got = open_login(&no_iap, &query(40_000, &challenge, "s"), None).await;
    assert_eq!(got.status, 404);
    no_iap.stop().await;
    on.stop().await;
}

fn manifest_body(key: &str) -> Value {
    json!({
        "repo": REPO,
        "key": key,
        "manifest": BASE64STD.encode(
            serde_json::to_vec(&json!({"schema": 1, "key": key, "target": "//a:b", "outputs": []})).unwrap()
        ),
    })
}

fn ci(git_ref: &str, job_workflow: Option<&str>) -> String {
    let mut claims = json!({"ref": git_ref});
    if let Some(workflow) = job_workflow {
        claims["job_workflow_ref"] = json!(workflow);
    }
    format!("Bearer {}", token_with_claims(REPO, claims))
}

async fn upload_results(server: &Server, bearer: &str) -> u16 {
    use aster_results_model::{to_ndjson, Record, RunRecord};
    let body = to_ndjson(&[Record::Run(RunRecord {
        run_id: "run-1".into(),
        repo: REPO.into(),
        sha: "9f1c".into(),
        branch: Some("main".into()),
        pr: None,
        workflow: Some("CI".into()),
        job: Some("test".into()),
        attempt: Some(1),
        lane: None,
        runner: None,
        aster_version: "0.24.0".into(),
        started_at: "2026-10-13T17:02:11Z".into(),
    })]);
    browser()
        .post(format!("{}/v1/test-results/runs", server.url))
        .header("authorization", bearer)
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn ci_may_write_only_from_the_default_branch_and_the_named_workflow() {
    let Some(server) = start_with(&[(
        "ASTER_SRV_CACHE_WRITER_WORKFLOWS",
        "ArchAstro/firstlanding/.github/workflows/ci.yml@",
    )])
    .await
    else {
        return;
    };
    let write = |bearer: String, key: &'static str| {
        let server = &server;
        async move {
            server
                .post_json("/v1/cache/commit", &manifest_body(key), Some(&bearer), None)
                .await
                .0
        }
    };
    let pr = "refs/pull/7/merge";
    let main = "refs/heads/main";
    assert_eq!(
        write(ci(main, Some(CI_WORKFLOW)), "k-main").await,
        201,
        "main from ci.yml"
    );
    assert_eq!(
        write(ci(pr, Some(CI_WORKFLOW)), "k-pr").await,
        403,
        "a PR ref from ci.yml"
    );
    assert_eq!(
        write(
            ci(
                main,
                Some("ArchAstro/firstlanding/.github/workflows/evil.yml@refs/heads/main")
            ),
            "k-evil"
        )
        .await,
        403,
        "main from another workflow"
    );
    assert_eq!(
        write(
            ci(
                main,
                Some("ArchAstro/firstlanding/.github/workflows/ci.yml.evil@refs/heads/main")
            ),
            "k-lookalike"
        )
        .await,
        403,
        "a workflow whose path only starts like the allowed one"
    );
    assert_eq!(
        write(ci(main, None), "k-none").await,
        403,
        "main with no workflow claim"
    );
    // Reads are not limited by workflow.
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k-main"]}),
            Some(&ci(pr, Some("x/y/.github/workflows/z.yml@r"))),
            None,
        )
        .await;
    assert_eq!(status, 200);

    // Test-results uploads follow the same workflow rule (any ref).
    assert_eq!(
        upload_results(&server, &ci(main, Some(CI_WORKFLOW))).await,
        201
    );
    assert_eq!(
        upload_results(&server, &ci(pr, Some(CI_WORKFLOW))).await,
        200,
        "a replayed run"
    );
    assert_eq!(
        upload_results(
            &server,
            &ci(
                main,
                Some("ArchAstro/firstlanding/.github/workflows/evil.yml@refs/heads/main")
            )
        )
        .await,
        403
    );
    assert_eq!(upload_results(&server, &ci(main, None)).await, 403);
    server.stop().await;
}

#[tokio::test]
async fn without_writer_workflows_configured_any_workflow_on_the_default_branch_writes() {
    let Some(server) = start().await else {
        return;
    };
    let (status, _) = server
        .post_json(
            "/v1/cache/commit",
            &manifest_body("k-any"),
            Some(&ci(
                "refs/heads/main",
                Some("ArchAstro/other/.github/workflows/x.yml@r"),
            )),
            None,
        )
        .await;
    assert_eq!(status, 201);
    let (status, _) = server
        .post_json(
            "/v1/cache/commit",
            &manifest_body("k-none"),
            Some(&ci("refs/heads/main", None)),
            None,
        )
        .await;
    assert_eq!(status, 201, "a token with no workflow claim keeps working");
    assert_eq!(
        upload_results(&server, &ci("refs/heads/main", None)).await,
        201
    );
    server.stop().await;
}

#[tokio::test]
async fn an_older_ci_client_with_a_proxy_header_still_reads_and_writes() {
    let Some(server) = start().await else {
        return;
    };
    // 0.22/0.23 sends the OIDC bearer plus a Proxy-Authorization the server never looks at.
    let bearer = ci("refs/heads/main", Some(CI_WORKFLOW));
    for (path, body, expect) in [
        (
            "/v1/cache/lookup",
            json!({"repo": REPO, "keys": ["k"]}),
            200,
        ),
        ("/v1/cache/commit", manifest_body("k-old"), 201),
    ] {
        let response = browser()
            .post(format!("{}{path}", server.url))
            .header("authorization", &bearer)
            .header("proxy-authorization", "Bearer an-old-iap-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expect, "{path}");
    }
    // The header is no credential of its own.
    let response = browser()
        .post(format!("{}/v1/cache/lookup", server.url))
        .header("proxy-authorization", "Bearer an-old-iap-token")
        .json(&json!({"repo": REPO, "keys": ["k"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    server.stop().await;
}
