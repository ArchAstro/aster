//! Copy-code sign-in (`/cli-auth?mode=copy`) and the rest of the sign-in
//! hardening: mode binding, the user code, origin and IAP-age checks, PKCE
//! method, revocation and the kill switch. Real Postgres; the browser is a
//! scripted HTTP client that attaches a signed IAP assertion.
//!
//! Set `ASTER_SRV_TEST_DATABASE_URL`; without it the tests skip.

mod common;

use std::sync::Arc;

use aster_srv::cache::{BlobStore, FixedSigner, Signer};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use common::{
    iap_assertion, iap_assertion_issued, person_bearer, Server, IAP_AUDIENCE, PUBLIC_URL,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const REPO: &str = "ArchAstro/firstlanding";
const ADA: &str = "ada@example.com";
const USER_CODE: &str = "ABCD2345";

async fn start_with(extra: &[(&str, &str)]) -> Option<Server> {
    let signer = Arc::new(FixedSigner {
        email: "cache-test@example.iam.gserviceaccount.com".into(),
        signature: b"test-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory("aster-cache-test", signer as Arc<dyn Signer>);
    let mut env = vec![
        ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
        (
            "ASTER_SRV_CACHE_SIGNER",
            "cache-test@example.iam.gserviceaccount.com",
        ),
        ("ASTER_SRV_IAP_AUDIENCE", IAP_AUDIENCE),
        ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
    ];
    env.extend_from_slice(extra);
    env.reverse();
    common::start_with(&env, Some(store)).await
}

async fn start() -> Option<Server> {
    start_with(&[]).await
}

fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    let verifier = BASE64URL.encode(bytes);
    let challenge = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

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

fn browser() -> reqwest::Client {
    aster_srv::http_client()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn page(response: reqwest::Response) -> Page {
    Page {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        body: response.text().await.unwrap_or_default(),
    }
}

async fn open(server: &Server, query: &str, assertion: &str) -> Page {
    page(
        browser()
            .get(format!("{}/cli-auth?{query}", server.url))
            .header("x-goog-iap-jwt-assertion", assertion)
            .header("x-forwarded-for", "9.9.9.9, 203.0.113.7, 130.211.0.1")
            .header("user-agent", "TestBrowser/1.0 <b>")
            .send()
            .await
            .unwrap(),
    )
    .await
}

/// POST the form. `origin` and `referer` are what the browser would send.
async fn submit(
    server: &Server,
    fields: &[(String, String)],
    assertion: &str,
    origin: Option<&str>,
    referer: Option<&str>,
) -> Page {
    let body = fields
        .iter()
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut request = browser()
        .post(format!("{}/cli-auth", server.url))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-goog-iap-jwt-assertion", assertion)
        .body(body);
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    if let Some(referer) = referer {
        request = request.header("referer", referer);
    }
    page(request.send().await.unwrap()).await
}

async fn confirm(server: &Server, fields: &[(String, String)], assertion: &str) -> Page {
    submit(server, fields, assertion, Some(PUBLIC_URL), None).await
}

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

fn with(fields: &[(String, String)], name: &str, value: &str) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(n, v)| (n.clone(), if n == name { value.into() } else { v.clone() }))
        .collect()
}

fn copy_query(challenge: &str, state: &str) -> String {
    format!("mode=copy&challenge={challenge}&state={state}&user_code={USER_CODE}&challenge_method=S256&host=devbox-7")
}

fn loopback_query(port: u16, challenge: &str, state: &str) -> String {
    format!("port={port}&challenge={challenge}&state={state}&user_code={USER_CODE}")
}

async fn exchange(server: &Server, body: Value) -> (u16, Value) {
    server.post_json("/v1/auth/token", &body, None, None).await
}

fn invalid_grant() -> Value {
    json!({"error": "invalid_grant"})
}

/// The one-time code a copy-mode page shows, from `<pre><code>CODE</code></pre>`.
fn shown_code(html: &str) -> String {
    let rest = html.split("<pre><code>").nth(1).expect("a code block");
    rest.split("</code>").next().unwrap().to_string()
}

/// `POST /v1/auth/revoke` with no body, as `aster srv logout` sends it
/// (the route takes none).
async fn revoke(server: &Server, bearer: Option<&str>) -> (u16, Value) {
    let mut request = aster_srv::http_client()
        .build()
        .unwrap()
        .post(format!("{}/v1/auth/revoke", server.url));
    if let Some(bearer) = bearer {
        request = request.header("Authorization", bearer);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(json!(null)))
}

async fn code_rows(server: &Server) -> i64 {
    server
        .client()
        .await
        .query_one("SELECT count(*) FROM auth.cli_codes", &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn copy_mode_shows_a_one_time_code_and_never_redirects_or_shows_a_token() {
    let Some(server) = start().await else {
        return;
    };
    let ada = iap_assertion(ADA);
    let (verifier, challenge) = pkce();

    // The confirm page names who, from where, and the code the terminal printed.
    let form = open(&server, &copy_query(&challenge, "state-copy-1"), &ada).await;
    assert_eq!(form.status, 200, "{}", form.body);
    assert!(form.body.contains("ABCD-2345"), "{}", form.body);
    assert!(form.body.contains("devbox-7"), "{}", form.body);
    assert!(form.body.contains(ADA), "{}", form.body);
    // The client address is the one the load balancer appended, not the
    // client's own first entry; the user agent is shown escaped.
    assert!(form.body.contains("203.0.113.7"), "{}", form.body);
    assert!(!form.body.contains("9.9.9.9"), "{}", form.body);
    assert!(
        form.body.contains("TestBrowser/1.0 &lt;b&gt;"),
        "{}",
        form.body
    );
    // Each detail says who vouches for it: the device name is typed by the
    // requesting command, so the page says it is not verified.
    assert!(
        form.body.contains(
            "Device name (reported by the requesting computer, not verified): <code>devbox-7</code>"
        ),
        "{}",
        form.body
    );
    assert!(
        form.body
            .contains("Your browser's address (seen by this server)"),
        "{}",
        form.body
    );
    assert!(
        form.body.contains("Your browser (as it describes itself)"),
        "{}",
        form.body
    );
    assert!(
        form.body
            .contains("Only continue if you started this command yourself"),
        "copy mode warns on the confirm page: {}",
        form.body
    );
    assert!(
        !form.body.contains("name=\"port\"") && !form.body.contains("(port"),
        "copy mode has no port: {}",
        form.body
    );
    let fields = form_fields(&form.body);
    let names: Vec<_> = fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["user_code", "challenge", "state", "exp", "mac", "mode"]
    );

    // Authorize: a page with the code, never a redirect.
    let done = confirm(&server, &fields, &ada).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert!(done.location().is_none(), "copy mode must not redirect");
    assert!(!done.body.contains("127.0.0.1"), "{}", done.body);
    let code = shown_code(&done.body);
    assert_eq!(code.len(), 43, "a 256-bit code, base64url");
    assert!(done.body.contains("state-co"), "the request id is shown");
    assert!(
        done.body.contains("Never send this code to anyone."),
        "{}",
        done.body
    );
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "same-origin"),
        ("x-content-type-options", "nosniff"),
    ] {
        assert_eq!(done.headers[name], value, "{name}");
    }
    let csp = done.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(csp.contains("default-src 'none'"), "{csp}");

    // The row lives at most 60 seconds, the same as a loopback code.
    let ttl: f64 = server
        .client()
        .await
        .query_one(
            "SELECT extract(epoch FROM expires_at - now())::float8 FROM auth.cli_codes",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(ttl > 0.0 && ttl <= 60.0, "{ttl}");

    // The code and verifier exchange for a token, with no port.
    let body = json!({"code": code, "verifier": verifier, "user_code": USER_CODE});
    let (status, reply) = exchange(&server, body.clone()).await;
    assert_eq!(status, 200, "{reply}");
    let token = reply["token"].as_str().unwrap().to_string();
    assert_eq!(reply["email"], ADA);
    // The token never appeared on a page.
    assert!(!done.body.contains(&token) && !form.body.contains(&token));
    assert!(!done.body.contains("eyJ"), "no JWT on the page");
    // It reads the cache.
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&format!("Bearer {token}")),
            None,
        )
        .await;
    assert_eq!(status, 200);
    // Replaying the code fails.
    assert_eq!(exchange(&server, body).await, (400, invalid_grant()));
    server.stop().await;
}

#[tokio::test]
async fn a_code_is_bound_to_its_mode_user_code_challenge_and_verifier() {
    let Some(server) = start().await else {
        return;
    };
    let ada = iap_assertion(ADA);
    let (verifier, challenge) = pkce();
    let mint_copy = |state: &'static str| {
        let (server, ada, challenge) = (&server, &ada, challenge.clone());
        async move {
            let form = open(server, &copy_query(&challenge, state), ada).await;
            let done = confirm(server, &form_fields(&form.body), ada).await;
            shown_code(&done.body)
        }
    };
    let mint_loopback = |state: &'static str| {
        let (server, ada, challenge) = (&server, &ada, challenge.clone());
        async move {
            let form = open(server, &loopback_query(40_321, &challenge, state), ada).await;
            let done = confirm(server, &form_fields(&form.body), ada).await;
            assert_eq!(done.status, 302, "{}", done.body);
            let location = done.location().unwrap().to_string();
            location
                .split("code=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
                .to_string()
        }
    };

    // A copy code is useless to a loopback exchange (with a port)...
    let code = mint_copy("s-1").await;
    assert_eq!(
        exchange(
            &server,
            json!({"code": code, "verifier": verifier, "user_code": USER_CODE, "port": 40_321})
        )
        .await,
        (400, invalid_grant())
    );
    // ...and a loopback code is useless without its port, or with port 0.
    for body in [
        json!({"verifier": verifier, "user_code": USER_CODE}),
        json!({"verifier": verifier, "user_code": USER_CODE, "port": 0}),
    ] {
        let code = mint_loopback("s-2").await;
        let mut body = body;
        body["code"] = json!(code);
        assert_eq!(
            exchange(&server, body.clone()).await,
            (400, invalid_grant()),
            "{body}"
        );
    }
    // A wrong user code, a wrong verifier: refused, and the code is spent.
    for (what, body) in [
        (
            "wrong user code",
            json!({"verifier": verifier, "user_code": "ZZZZ9999"}),
        ),
        ("no user code", json!({"verifier": verifier})),
        (
            "wrong verifier",
            json!({"verifier": pkce().0, "user_code": USER_CODE}),
        ),
    ] {
        let code = mint_copy("s-3").await;
        let mut body = body;
        body["code"] = json!(code);
        assert_eq!(
            exchange(&server, body.clone()).await,
            (400, invalid_grant()),
            "{what}"
        );
        // A body that does not parse is refused before it touches the row.
        if what == "no user code" {
            continue;
        }
        let mut right = json!({"verifier": verifier, "user_code": USER_CODE});
        right["code"] = json!(code);
        assert_eq!(
            exchange(&server, right).await,
            (400, invalid_grant()),
            "{what}: the refused attempt spent the code"
        );
    }
    // The right pair works.
    let code = mint_copy("s-4").await;
    let (status, _) = exchange(
        &server,
        json!({"code": code, "verifier": verifier, "user_code": USER_CODE}),
    )
    .await;
    assert_eq!(status, 200);
    server.stop().await;
}

#[tokio::test]
async fn a_confirm_form_for_one_mode_cannot_be_submitted_as_the_other() {
    let Some(server) = start().await else {
        return;
    };
    let ada = iap_assertion(ADA);
    let (_, challenge) = pkce();
    let loopback = form_fields(
        &open(&server, &loopback_query(40_000, &challenge, "s"), &ada)
            .await
            .body,
    );
    let copy = form_fields(&open(&server, &copy_query(&challenge, "s"), &ada).await.body);

    let mut attempts: Vec<(&str, Vec<(String, String)>)> = vec![
        // Loopback form re-labelled copy, with and without its port.
        ("loopback form as copy", {
            let mut f = loopback.clone();
            f.push(("mode".into(), "copy".into()));
            f
        }),
        ("loopback form as copy, port dropped", {
            let mut f: Vec<_> = loopback
                .iter()
                .filter(|(n, _)| n != "port")
                .cloned()
                .collect();
            f.push(("mode".into(), "copy".into()));
            f
        }),
        // Copy form given a port, or switched to loopback.
        ("copy form with a port", {
            let mut f = copy.clone();
            f.push(("port".into(), "40000".into()));
            f
        }),
        ("copy form as loopback", {
            let mut f: Vec<_> = copy.iter().filter(|(n, _)| n != "mode").cloned().collect();
            f.push(("port".into(), "40000".into()));
            f
        }),
        (
            "copy form, mode removed",
            copy.iter().filter(|(n, _)| n != "mode").cloned().collect(),
        ),
        ("copy form, unknown mode", with(&copy, "mode", "device")),
        ("tampered user code", with(&copy, "user_code", "ZZZZ9999")),
        (
            "loopback tampered user code",
            with(&loopback, "user_code", "ZZZZ9999"),
        ),
        (
            "user code removed",
            copy.iter()
                .filter(|(n, _)| n != "user_code")
                .cloned()
                .collect(),
        ),
    ];
    attempts.push((
        "bad user code alphabet",
        with(&copy, "user_code", "ABCD0O1I"),
    ));
    for (what, fields) in &attempts {
        let got = confirm(&server, fields, &ada).await;
        assert_eq!(got.status, 400, "{what}: {}", got.body);
        assert!(got.location().is_none(), "{what} must not redirect");
        assert!(!got.body.contains("<pre>"), "{what} must not show a code");
    }
    assert_eq!(
        code_rows(&server).await,
        0,
        "no refused form may mint a code"
    );
    server.stop().await;
}

#[tokio::test]
async fn copy_mode_and_pkce_requests_are_validated() {
    let Some(server) = start().await else {
        return;
    };
    let ada = iap_assertion(ADA);
    let (_, challenge) = pkce();
    let base = format!("challenge={challenge}&state=s&user_code={USER_CODE}");
    for (what, query) in [
        ("copy with a port", format!("mode=copy&port=40000&{base}")),
        ("unknown mode", format!("mode=device&{base}")),
        (
            "plain challenge method",
            format!("mode=copy&challenge_method=plain&{base}"),
        ),
        (
            "lowercase s256",
            format!("mode=copy&challenge_method=s256&{base}"),
        ),
        (
            "loopback with plain",
            format!("port=40000&challenge_method=plain&{base}"),
        ),
        (
            "no user code",
            format!("mode=copy&challenge={challenge}&state=s"),
        ),
        (
            "short user code",
            format!("mode=copy&challenge={challenge}&state=s&user_code=ABCD"),
        ),
        ("host with markup", format!("mode=copy&host=%3Cb%3E&{base}")),
        (
            "long host",
            format!("mode=copy&host={}&{base}", "h".repeat(65)),
        ),
        (
            "no state",
            format!("mode=copy&challenge={challenge}&user_code={USER_CODE}"),
        ),
    ] {
        let got = open(&server, &query, &ada).await;
        assert_eq!(got.status, 400, "{what}: {}", got.body);
    }
    for query in [
        format!("mode=copy&challenge_method=S256&{base}"),
        format!("mode=loopback&port=40000&{base}"),
        format!("mode=copy&{base}"),
    ] {
        assert_eq!(open(&server, &query, &ada).await.status, 200, "{query}");
    }
    server.stop().await;
}

#[tokio::test]
async fn the_confirm_post_must_come_from_the_servers_own_origin() {
    let Some(server) = start().await else {
        return;
    };
    let ada = iap_assertion(ADA);
    let (_, challenge) = pkce();
    let form = form_fields(&open(&server, &copy_query(&challenge, "s"), &ada).await.body);
    let own_referer = format!("{PUBLIC_URL}/cli-auth?mode=copy");
    for (what, origin, referer) in [
        ("no origin or referer", None, None),
        ("another site", Some("https://evil.example"), None),
        ("null origin", Some("null"), None),
        (
            "a look-alike host",
            Some("https://aster-srv.test.evil.example"),
            None,
        ),
        (
            "plain http for the https server",
            Some("http://aster-srv.test"),
            None,
        ),
        (
            "another referer",
            None,
            Some("https://evil.example/cli-auth"),
        ),
        (
            "a referer that only starts with our host",
            None,
            Some("https://aster-srv.test.evil.example/"),
        ),
        (
            "a wrong origin the right referer cannot rescue",
            Some("https://evil.example"),
            Some(own_referer.as_str()),
        ),
    ] {
        let got = submit(&server, &form, &ada, origin, referer).await;
        assert_eq!(got.status, 403, "{what}: {}", got.body);
        assert!(got.location().is_none());
    }
    assert_eq!(code_rows(&server).await, 0);
    // The server's own origin, or (with no Origin header) its own referer.
    let ok = submit(&server, &form, &ada, Some(PUBLIC_URL), None).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    let form = form_fields(
        &open(&server, &copy_query(&challenge, "s2"), &ada)
            .await
            .body,
    );
    let ok = submit(&server, &form, &ada, None, Some(&own_referer)).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    server.stop().await;
}

#[tokio::test]
async fn an_old_iap_assertion_cannot_mint_a_code() {
    let Some(server) = start().await else {
        return;
    };
    let (_, challenge) = pkce();
    let now = chrono::Utc::now().timestamp();
    let audience = IAP_AUDIENCE;
    let stale = iap_assertion_issued(ADA, audience, now - 600);
    let future = iap_assertion_issued(ADA, audience, now + 3600);
    let fresh_enough = iap_assertion_issued(ADA, audience, now - 240);
    let q = copy_query(&challenge, "s");
    for (what, assertion) in [
        ("ten minutes old", &stale),
        ("issued in the future", &future),
    ] {
        let got = open(&server, &q, assertion).await;
        assert_eq!(got.status, 401, "GET, {what}");
        let fields = form_fields(&open(&server, &q, &iap_assertion(ADA)).await.body);
        let got = confirm(&server, &fields, assertion).await;
        assert_eq!(got.status, 401, "POST, {what}: {}", got.body);
    }
    assert_eq!(code_rows(&server).await, 0);
    // An assertion with no `iat` at all is not recent either.
    let no_iat = {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        // Reuse a real assertion's signature input is impossible without
        // the key, so a header-only forgery is refused by the signature
        // check before the age check even runs.
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("iap".into());
        encode(
            &header,
            &json!({"iss": "https://cloud.google.com/iap", "aud": audience,
                    "exp": now + 300, "email": ADA}),
            &EncodingKey::from_secret(b"x"),
        )
        .unwrap()
    };
    assert_eq!(open(&server, &q, &no_iat).await.status, 401);
    // Within five minutes it works.
    assert_eq!(open(&server, &q, &fresh_enough).await.status, 200);
    let fields = form_fields(&open(&server, &q, &fresh_enough).await.body);
    assert_eq!(confirm(&server, &fields, &fresh_enough).await.status, 200);
    server.stop().await;
}

#[tokio::test]
async fn logout_refuses_every_earlier_token_for_that_person_only() {
    let Some(server) = start().await else {
        return;
    };
    let lookup = |token: String| {
        let server = &server;
        async move {
            server
                .post_json(
                    "/v1/cache/lookup",
                    &json!({"repo": REPO, "keys": ["k"]}),
                    Some(&token),
                    None,
                )
                .await
                .0
        }
    };
    let first = person_bearer(&server, ADA);
    let second_device = person_bearer(&server, ADA);
    let bob = person_bearer(&server, "bob@example.com");
    assert_eq!(lookup(first.clone()).await, 200);

    // No bearer, or a CI token, cannot revoke anyone.
    let (status, _) = revoke(&server, None).await;
    assert_eq!(status, 401);
    let ci = format!("Bearer {}", common::token(REPO));
    let (status, _) = revoke(&server, Some(&ci)).await;
    assert_eq!(status, 403);
    assert_eq!(
        lookup(first.clone()).await,
        200,
        "a refused revoke changes nothing"
    );

    let (status, reply) = revoke(&server, Some(&first)).await;
    assert_eq!((status, reply), (200, json!({"revoked": true})));
    // Both of Ada's earlier tokens are refused, Bob's is not.
    assert_eq!(lookup(first.clone()).await, 401);
    assert_eq!(lookup(second_device).await, 401);
    assert_eq!(lookup(bob).await, 200);
    // The revoked token cannot revoke again.
    let (status, _) = revoke(&server, Some(&first)).await;
    assert_eq!(status, 401);

    // A token issued after the cutoff works. `iat` has second resolution and
    // the cutoff rounds up, so wait out the second.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    assert_eq!(lookup(person_bearer(&server, ADA)).await, 200);
    // A different replica on the same database sees the revocation.
    let other = server.second(&[]).await;
    let (status, _) = other
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&first),
            None,
        )
        .await;
    assert_eq!(status, 401);
    other.stop().await;
    server.stop().await;
}

#[tokio::test]
async fn logout_follows_the_stable_subject_when_the_email_changes_and_is_audited() {
    let Some(server) = start().await else {
        return;
    };
    aster_srv::log::start_capture();
    let issuer = server.app.person_tokens.as_ref().unwrap();
    let subject = common::iap_subject(ADA);
    // The same IAP account before and after an email rename, and another person.
    let old_email = format!("Bearer {}", issuer.issue(&subject, ADA).unwrap().token);
    let renamed = format!(
        "Bearer {}",
        issuer.issue(&subject, "ada.new@example.com").unwrap().token
    );
    let bob = person_bearer(&server, "bob@example.com");

    // Signing out with one token revokes the subject's other token, whatever
    // email it carries.
    let (status, _) = revoke(&server, Some(&renamed)).await;
    assert_eq!(status, 200);
    let (status, _) = revoke(&server, Some(&old_email)).await;
    assert_eq!(status, 401, "the old-email token is revoked by subject");
    let (status, _) = revoke(&server, Some(&bob)).await;
    assert_eq!(
        status, 200,
        "another subject is untouched until it signs out"
    );

    // The audit hook records the subject, not the email.
    let line = aster_srv::log::captured()
        .into_iter()
        .find(|line| line.contains("\"token_revoke\"") && line.contains(&subject))
        .expect("a token_revoke audit line");
    assert!(!line.contains("ada.new@example.com"), "{line}");
    server.stop().await;
}

#[tokio::test]
async fn the_kill_switch_turns_off_sign_in_and_every_person_token() {
    let Some(on) = start().await else {
        return;
    };
    let token = person_bearer(&on, ADA);
    let off = on.second(&[("ASTER_SRV_PERSON_TOKENS", "off")]).await;
    let (_, challenge) = pkce();
    let got = open(&off, &copy_query(&challenge, "s"), &iap_assertion(ADA)).await;
    assert_eq!(got.status, 404);
    let (status, _) = exchange(
        &off,
        json!({"code": "x", "verifier": pkce().0, "user_code": USER_CODE}),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = revoke(&off, Some(&token)).await;
    assert_eq!(status, 404);
    let (status, _) = off
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, 401, "a token issued while it was on is refused");
    // Back on, the same token works again.
    assert_eq!(
        on.post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&token),
            None,
        )
        .await
        .0,
        200
    );
    off.stop().await;
    on.stop().await;
}
