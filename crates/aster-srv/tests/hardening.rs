//! The non-IAP `/v1` channel's hardening, against real Postgres and real
//! sockets: the dedicated API listener, rate and size limits, timeouts and
//! connection caps, audit lines and metrics, person-token binding, and
//! response headers.
//!
//! The canonical end-to-end proof is
//! `api_listener_serves_only_v1_while_the_main_port_is_unchanged`: it starts
//! both listeners of one server on real TCP ports, signs a person in through
//! the main port (IAP side), redeems and uses the token on the API port, and
//! walks the path matrix on both.
//!
//! Set `ASTER_SRV_TEST_DATABASE_URL`; without it the tests skip.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aster_srv::cache::{BlobStore, FixedSigner, Signer};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use common::{iap_assertion, iap_assertion_with_subject, iap_subject, token, Server};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const REPO: &str = "ArchAstro/firstlanding";
/// What the CLI would print and send; the confirm page shows it.
const USER_CODE: &str = "ABCD2345";
const HSTS: &str = "max-age=31536000; includeSubDomains";

async fn start_with(extra: &[(&str, &str)]) -> Option<Server> {
    let signer = Arc::new(FixedSigner {
        email: "cache-test@example.iam.gserviceaccount.com".into(),
        signature: b"test-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory("aster-cache-test", signer as Arc<dyn Signer>);
    let mut env: Vec<(&str, &str)> = extra.to_vec();
    env.extend_from_slice(&[
        ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
        (
            "ASTER_SRV_CACHE_SIGNER",
            "cache-test@example.iam.gserviceaccount.com",
        ),
        (
            "ASTER_SRV_IAP_AUDIENCE",
            "/projects/123456/global/backendServices/4242",
        ),
        ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
    ]);
    aster_srv::log::start_capture();
    common::start_with(&env, Some(store)).await
}

fn client() -> reqwest::Client {
    aster_srv::http_client()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    let verifier = BASE64URL.encode(bytes);
    let challenge = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// The code a person gets by confirming `/cli-auth` on `main_url` (the
/// browser side, behind IAP), and the verifier that redeems it.
async fn browser_login(main_url: &str, assertion: &str) -> (String, String) {
    let (verifier, challenge) = pkce();
    let http = client();
    let form = http
        .get(format!(
            "{main_url}/cli-auth?port=40123&user_code={USER_CODE}&challenge={challenge}&state=s1"
        ))
        .header("x-goog-iap-jwt-assertion", assertion)
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 200);
    let html = form.text().await.unwrap();
    let body = html
        .split("<input type=\"hidden\" name=\"")
        .skip(1)
        .map(|part| {
            let (name, rest) = part.split_once("\" value=\"").unwrap();
            format!("{name}={}", rest.split_once('"').unwrap().0)
        })
        .collect::<Vec<_>>()
        .join("&");
    let done = http
        .post(format!("{main_url}/cli-auth"))
        .header("content-type", "application/x-www-form-urlencoded")
        // A browser sends its own page's origin with a form POST.
        .header("origin", common::PUBLIC_URL)
        .header("x-goog-iap-jwt-assertion", assertion)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(done.status(), 302);
    let location = done.headers()["location"].to_str().unwrap().to_string();
    let code = location
        .strip_prefix("http://127.0.0.1:40123/callback?code=")
        .unwrap()
        .split_once("&state=")
        .unwrap()
        .0
        .to_string();
    (code, verifier)
}

async fn redeem(url: &str, code: &str, verifier: &str) -> (u16, Value) {
    let response = client()
        .post(format!("{url}/v1/auth/token"))
        .json(&json!({"code": code, "verifier": verifier, "port": 40123, "user_code": USER_CODE}))
        .send()
        .await
        .unwrap();
    (
        response.status().as_u16(),
        response.json().await.unwrap_or(Value::Null),
    )
}

/// A well-formed exchange for a code that does not exist.
async fn bad_exchange(url: &str, forwarded_for: Option<&str>) -> reqwest::Response {
    let (verifier, _) = pkce();
    let mut request = client()
        .post(format!("{url}/v1/auth/token"))
        .json(&json!({"code": BASE64URL.encode([7u8; 32]), "verifier": verifier, "port": 40123, "user_code": USER_CODE}));
    if let Some(forwarded_for) = forwarded_for {
        request = request.header("x-forwarded-for", forwarded_for);
    }
    request.send().await.unwrap()
}

async fn lookup(url: &str, bearer: &str) -> reqwest::Response {
    client()
        .post(format!("{url}/v1/cache/lookup"))
        .header("authorization", bearer)
        .json(&json!({"repo": REPO, "keys": ["k"]}))
        .send()
        .await
        .unwrap()
}

fn ci_bearer() -> String {
    format!("Bearer {}", token(REPO))
}

/// Send raw bytes, read until the server closes or `wait` passes.
async fn raw(url: &str, request: &[u8], wait: Duration) -> String {
    let mut stream = tokio::net::TcpStream::connect(url.trim_start_matches("http://"))
        .await
        .unwrap();
    let _ = stream.write_all(request).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(wait, async {
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buffer[..n]),
            }
        }
    })
    .await;
    String::from_utf8_lossy(&out).into_owned()
}

fn status_of(response: &str) -> Option<u16> {
    response.split_whitespace().nth(1)?.parse().ok()
}

// ---------------------------------------------------------------- item 1

#[tokio::test]
async fn api_listener_serves_only_v1_while_the_main_port_is_unchanged() {
    let Some(server) = start_with(&[("ASTER_SRV_API_ADDR", "127.0.0.1:0")]).await else {
        return;
    };
    let main = server.url.clone();
    let api = server.api_url.clone().expect("the API listener is on");
    assert_ne!(main, api);
    let http = client();
    let ada = iap_assertion("ada@example.com");

    // Phase 1: the main port serves what it always did.
    let get = |url: String| {
        let (http, ada) = (http.clone(), ada.clone());
        async move {
            http.get(url)
                .header("x-goog-iap-jwt-assertion", ada)
                .send()
                .await
                .unwrap()
        }
    };
    for (path, status) in [("/healthz", 200), ("/readyz", 200), ("/metrics", 200)] {
        let direct = http.get(format!("{main}{path}")).send().await.unwrap();
        assert_eq!(direct.status(), status, "{path}");
    }
    // (/metrics with an IAP header is the outside view; see its own test.)
    assert_eq!(
        get(format!("{main}/metrics")).await.status(),
        404,
        "/metrics through IAP is hidden"
    );
    assert_eq!(get(format!("{main}/ui")).await.status(), 200);
    // The API listener's bare health check is not a route here. Any `/v1/`
    // path outside the route table is behind the auth gate, so without a
    // token the main port answers 401, not the API port's 200.
    assert_eq!(get(format!("{main}/v1/healthz")).await.status(), 401);

    // Phase 2: a person signs in on the main port, the CLI redeems and uses
    // the token on the API port.
    let (code, verifier) = browser_login(&main, &ada).await;
    let (status, reply) = redeem(&api, &code, &verifier).await;
    assert_eq!(status, 200, "{reply}");
    let bearer = format!("Bearer {}", reply["token"].as_str().unwrap());
    assert_eq!(lookup(&api, &bearer).await.status(), 200);
    assert_eq!(
        lookup(&main, &bearer).await.status(),
        200,
        "main still serves /v1"
    );
    assert_eq!(lookup(&api, &ci_bearer()).await.status(), 200);

    // Phase 3: the API port's health check answers `ok` and nothing else.
    let health = http.get(format!("{api}/v1/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await.unwrap(), "ok");

    // Phase 4: everything outside /v1/ is a 404 on the API port, whatever
    // credentials ride along, for every method.
    let outside = [
        "/",
        "/ui",
        "/ui/",
        "/ui/cache?repo=ArchAstro%2Ffirstlanding",
        "/ui/tests",
        "/cli-auth",
        "/cli-auth?port=40123&user_code=ABCD2345&challenge=x&state=s",
        "/healthz",
        "/readyz",
        "/metrics",
        "/v1",
        "//v1/healthz",
        "/V1/healthz",
        "/%2e%2e/ui",
        "/robots.txt",
        "/aster-cache-test/cas/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "/internal/debug",
    ];
    for path in outside {
        for method in ["GET", "POST", "PUT", "DELETE"] {
            let response = http
                .request(method.parse().unwrap(), format!("{api}{path}"))
                .header("x-goog-iap-jwt-assertion", &ada)
                .header("authorization", &bearer)
                .body("x=1")
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let hsts = response.headers().get("strict-transport-security").cloned();
            let body = response.text().await.unwrap();
            assert_eq!(status, 404, "{method} {path}: {body}");
            assert!(!body.contains("<html"), "{method} {path} leaked a page");
            assert_eq!(hsts.unwrap(), HSTS);
        }
    }
    // Paths that begin with /v1/ but name no API route are behind the auth
    // gate: 401 without a token (the IAP header is not a token), never a
    // page. A client normalizes `/v1/../x` to `/x`, which is not /v1 at all.
    for (path, status) in [
        ("/v1/../ui", 404),
        ("/v1/../cli-auth", 404),
        ("/v1/nothing", 401),
        ("/v1/auth/other", 401),
    ] {
        let response = http
            .get(format!("{api}{path}"))
            .header("x-goog-iap-jwt-assertion", &ada)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
    }
    assert_eq!(
        http.post(format!("{api}/v1/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // The API port refuses a bad exchange like the main port does.
    assert_eq!(bad_exchange(&api, None).await.status(), 400);

    // Phase 5: the main port is unchanged after all of that.
    assert_eq!(get(format!("{main}/ui")).await.status(), 200);
    assert_eq!(
        http.get(format!("{main}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    server.stop().await;
}

#[tokio::test]
async fn without_api_addr_there_is_no_second_listener() {
    let Some(server) = start_with(&[]).await else {
        return;
    };
    assert!(server.api_url.is_none());
    server.stop().await;
}

// ---------------------------------------------------------------- item 3

#[tokio::test]
async fn token_exchange_is_limited_per_client_ip_and_answers_429_with_retry_after() {
    let Some(server) = start_with(&[("ASTER_SRV_RATE_TOKEN_IP_PER_MIN", "3")]).await else {
        return;
    };
    for attempt in 1..=3 {
        assert_eq!(
            bad_exchange(&server.url, None).await.status(),
            400,
            "{attempt}"
        );
    }
    let limited = bad_exchange(&server.url, None).await;
    assert_eq!(limited.status(), 429);
    let wait: u64 = limited.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(limited.headers()["strict-transport-security"], HSTS);
    assert_eq!(limited.headers()["cache-control"], "no-store");
    assert!(limited.text().await.unwrap().contains("too many requests"));

    // Google appends `<client>, <load balancer>`: the client is the second
    // entry from the right, so another real client is not limited...
    let other = bad_exchange(&server.url, Some("9.9.9.9, 198.51.100.7, 34.8.0.1")).await;
    assert_eq!(other.status(), 400);
    // ...and a caller cannot dodge the limit by writing a new left-hand entry.
    for spoof in ["1.1.1.1", "2.2.2.2", "3.3.3.3", "4.4.4.4"] {
        let forwarded = format!("{spoof}, 203.0.113.50, 34.8.0.1");
        let status = bad_exchange(&server.url, Some(&forwarded))
            .await
            .status()
            .as_u16();
        if spoof == "4.4.4.4" {
            assert_eq!(status, 429, "the fourth request from one real client");
        }
    }
    let metrics = server.app.metrics.render();
    assert!(
        metrics.contains("aster_srv_rate_limited_total{route=\"/v1/auth/token\",scope=\"ip\"}"),
        "{metrics}"
    );
    server.stop().await;
}

#[tokio::test]
async fn all_of_v1_is_limited_per_ip_and_per_subject_with_looser_defaults() {
    // Defaults are looser than the token route's.
    let Some(defaults) = start_with(&[]).await else {
        return;
    };
    assert!(
        defaults.app.config.rate_api_ip_per_min > 10 * defaults.app.config.rate_token_ip_per_min
    );
    defaults.stop().await;

    // Per subject: Ada is limited, Bob (same address) and CI are not.
    let Some(server) = start_with(&[("ASTER_SRV_RATE_API_SUBJECT_PER_MIN", "3")]).await else {
        return;
    };
    let ada = common::person_bearer(&server, "ada@example.com");
    let bob = common::person_bearer(&server, "bob@example.com");
    for _ in 0..3 {
        assert_eq!(lookup(&server.url, &ada).await.status(), 200);
    }
    let limited = lookup(&server.url, &ada).await;
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    assert_eq!(lookup(&server.url, &bob).await.status(), 200);
    assert_eq!(lookup(&server.url, &ci_bearer()).await.status(), 200);
    server.stop().await;

    // Per IP: the fifth request from one address in a minute, authenticated
    // or not, is refused.
    let Some(server) = start_with(&[("ASTER_SRV_RATE_API_IP_PER_MIN", "4")]).await else {
        return;
    };
    for _ in 0..4 {
        assert_eq!(lookup(&server.url, "Bearer nope").await.status(), 401);
    }
    let limited = lookup(&server.url, &ci_bearer()).await;
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    // Not /v1: the dashboard and health checks are not charged.
    for _ in 0..10 {
        let health = client()
            .get(format!("{}/healthz", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);
    }
    server.stop().await;
}

#[tokio::test]
async fn every_route_has_a_body_size_limit() {
    let Some(server) = start_with(&[("ASTER_SRV_MAX_BODY_BYTES", "1024")]).await else {
        return;
    };
    let http = client();
    let big = "x".repeat(9000);
    let medium = "x".repeat(2000);
    let ci = ci_bearer();
    // Small forms: 8 KiB, with or without credentials.
    for path in ["/v1/auth/token", "/cli-auth"] {
        let response = http
            .post(format!("{}{path}", server.url))
            .body(big.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413, "{path}");
    }
    // Uploads and cache writes: ASTER_SRV_MAX_BODY_BYTES, before auth.
    for (path, auth) in [
        ("/v1/test-results/runs", Some(&ci)),
        ("/v1/test-results/runs", None),
        ("/v1/cache/lookup", Some(&ci)),
        ("/v1/cache/commit", Some(&ci)),
        ("/v1/cache/upload", None),
    ] {
        let mut request = http
            .post(format!("{}{path}", server.url))
            .body(medium.clone());
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        assert_eq!(request.send().await.unwrap().status(), 413, "{path}");
    }
    // A chunked body has no Content-Length to refuse early; it is cut off
    // as it is read.
    let chunked = |path: &str, auth: &str| {
        let mut request =
            format!("POST {path} HTTP/1.1\r\nHost: x\r\n{auth}Transfer-Encoding: chunked\r\n\r\n");
        for _ in 0..20 {
            request.push_str(&format!("3e8\r\n{}\r\n", "x".repeat(1000)));
        }
        request.push_str("0\r\n\r\n");
        request
    };
    for (path, auth) in [
        ("/v1/cache/lookup", format!("authorization: {ci}\r\n")),
        ("/v1/auth/token", String::new()),
    ] {
        let reply = raw(
            &server.url,
            chunked(path, &auth).as_bytes(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(status_of(&reply), Some(413), "chunked {path}: {reply}");
    }
    // Routes that take no body refuse one.
    for path in [
        "/healthz",
        "/readyz",
        "/ui",
        "/v1/cache/summary?repo=ArchAstro/firstlanding",
        "/v1/test-results/tests",
    ] {
        let response = http
            .get(format!("{}{path}", server.url))
            .header("authorization", &ci)
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413, "GET {path} with a body");
    }
    // Within the limit, requests go through to their handlers.
    let small = http
        .post(format!("{}/v1/auth/token", server.url))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(small.status(), 400);
    assert_eq!(lookup(&server.url, &ci).await.status(), 200);
    server.stop().await;
}

#[tokio::test]
async fn a_slow_body_is_cut_off_by_the_body_timeout() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_API_ADDR", "127.0.0.1:0"),
        ("ASTER_SRV_BODY_TIMEOUT_SECS", "1"),
    ])
    .await
    else {
        return;
    };
    // Headers promise 100 bytes; one arrives, then the client stalls.
    let stalled = b"POST /v1/auth/token HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{";
    for url in [&server.url, server.api_url.as_ref().unwrap()] {
        let started = Instant::now();
        let reply = raw(url, stalled, Duration::from_secs(10)).await;
        assert_eq!(status_of(&reply), Some(408), "{url}: {reply}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(reply.to_lowercase().contains("strict-transport-security"));
    }
    // The server is still answering while clients stall.
    assert_eq!(
        client()
            .get(format!("{}/healthz", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    server.stop().await;
}

#[tokio::test]
async fn the_handler_timeout_ends_a_request_that_runs_too_long() {
    // The body timeout is longer than the handler's, so the handler timeout
    // is what ends a client that stalls mid-body.
    let Some(server) = start_with(&[
        ("ASTER_SRV_HANDLER_TIMEOUT_SECS", "1"),
        ("ASTER_SRV_BODY_TIMEOUT_SECS", "30"),
    ])
    .await
    else {
        return;
    };
    let stalled = b"POST /v1/auth/token HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{";
    let started = Instant::now();
    let reply = raw(&server.url, stalled, Duration::from_secs(10)).await;
    assert_eq!(status_of(&reply), Some(504), "{reply}");
    assert!(started.elapsed() < Duration::from_secs(5));
    server.stop().await;
}

#[tokio::test]
async fn a_client_that_trickles_headers_is_disconnected_on_both_listeners() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_API_ADDR", "127.0.0.1:0"),
        ("ASTER_SRV_HEADER_TIMEOUT_SECS", "1"),
    ])
    .await
    else {
        return;
    };
    for url in [&server.url, server.api_url.as_ref().unwrap()] {
        // The request line and one header, never the blank line.
        let started = Instant::now();
        let reply = raw(
            url,
            b"GET /v1/healthz HTTP/1.1\r\nHost: x\r\n",
            Duration::from_secs(10),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{url}: still open after {:?}",
            started.elapsed()
        );
        assert!(!reply.contains("200 OK"), "{reply}");
        // A client that sends nothing at all is dropped too.
        let started = Instant::now();
        raw(url, b"", Duration::from_secs(10)).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{url}: idle connection kept"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn the_connection_cap_makes_extra_clients_wait_until_a_slot_frees() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_MAX_CONNECTIONS", "2"),
        ("ASTER_SRV_HEADER_TIMEOUT_SECS", "60"),
    ])
    .await
    else {
        return;
    };
    let address = server.url.trim_start_matches("http://").to_string();
    let first = tokio::net::TcpStream::connect(&address).await.unwrap();
    let second = tokio::net::TcpStream::connect(&address).await.unwrap();
    // Let the server accept both before the third arrives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut third = tokio::net::TcpStream::connect(&address).await.unwrap();
    third
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buffer = [0u8; 1024];
    let early = tokio::time::timeout(Duration::from_millis(700), third.read(&mut buffer)).await;
    assert!(
        early.is_err(),
        "a third connection must not be served at the cap"
    );
    drop(first);
    let n = tokio::time::timeout(Duration::from_secs(5), third.read(&mut buffer))
        .await
        .expect("served once a slot is free")
        .unwrap();
    assert!(String::from_utf8_lossy(&buffer[..n]).starts_with("HTTP/1.1 200"));
    drop(second);
    server.stop().await;
}

#[tokio::test]
async fn a_flood_of_exchanges_cannot_starve_the_shared_pool_and_a_starved_pool_fails_fast() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_RATE_TOKEN_IP_PER_MIN", "100000"),
        ("ASTER_SRV_DB_WAIT_SECS", "1"),
    ])
    .await
    else {
        return;
    };
    let url = server.url.clone();
    let ci = ci_bearer();

    // A flood of well-formed but unknown codes, while CI keeps working.
    let flood = tokio::spawn({
        let url = url.clone();
        async move {
            let mut set = tokio::task::JoinSet::new();
            for _ in 0..150 {
                let url = url.clone();
                set.spawn(async move { bad_exchange(&url, None).await.status().as_u16() });
            }
            let mut statuses = Vec::new();
            while let Some(status) = set.join_next().await {
                statuses.push(status.unwrap());
            }
            statuses
        }
    });
    for round in 0..15 {
        let started = Instant::now();
        let status = lookup(&url, &ci).await.status();
        assert_eq!(status, 200, "CI lookup {round} during the flood");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }
    let statuses = flood.await.unwrap();
    assert!(
        statuses.iter().all(|s| matches!(s, 400 | 503)),
        "a flood is refused or told to retry, never a server error: {statuses:?}"
    );
    assert!(statuses.contains(&400));

    // Both auth connections held: exchanges fail, CI is unaffected, and
    // malformed requests never needed a connection at all.
    let held = [
        server.app.auth_pool.get().await.unwrap(),
        server.app.auth_pool.get().await.unwrap(),
    ];
    let started = Instant::now();
    assert_eq!(lookup(&url, &ci).await.status(), 200);
    assert!(started.elapsed() < Duration::from_millis(900));
    let malformed = client()
        .post(format!("{url}/v1/auth/token"))
        .json(&json!({"code": "short", "verifier": "short", "port": 1, "user_code": USER_CODE}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        malformed.status(),
        400,
        "refused from its shape, with no connection"
    );
    assert!(started.elapsed() < Duration::from_millis(1500));
    let started = Instant::now();
    let starved = bad_exchange(&url, None).await;
    assert_eq!(starved.status(), 500, "no auth connection within the wait");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    drop(held);
    assert_eq!(bad_exchange(&url, None).await.status(), 400, "recovers");

    // The shared pool exhausted: requests wait the configured time and fail
    // instead of hanging, and the auth pool and health check still answer.
    let mut shared = Vec::new();
    for _ in 0..16 {
        shared.push(server.app.pool.get().await.unwrap());
    }
    let started = Instant::now();
    let failed = lookup(&url, &ci).await;
    assert!(failed.status().is_server_error(), "{}", failed.status());
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(bad_exchange(&url, None).await.status(), 400);
    assert_eq!(
        client()
            .get(format!("{url}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    drop(shared);
    assert_eq!(lookup(&url, &ci).await.status(), 200);
    server.stop().await;
}

#[tokio::test]
async fn only_a_few_exchanges_run_at_once_and_the_rest_are_told_to_retry() {
    let Some(server) = start_with(&[("ASTER_SRV_RATE_TOKEN_IP_PER_MIN", "100000")]).await else {
        return;
    };
    // Hold the auth pool so running exchanges pile up behind it.
    let _held = [
        server.app.auth_pool.get().await.unwrap(),
        server.app.auth_pool.get().await.unwrap(),
    ];
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let url = server.url.clone();
        set.spawn(async move {
            let response = bad_exchange(&url, None).await;
            (
                response.status().as_u16(),
                response.headers().get("retry-after").is_some(),
            )
        });
    }
    let mut busy = 0;
    while let Some(result) = set.join_next().await {
        let (status, retry_after) = result.unwrap();
        if status == 503 {
            assert!(retry_after);
            busy += 1;
        }
    }
    assert!(
        busy >= 12 - aster_srv::limits::MAX_CONCURRENT_EXCHANGES,
        "{busy}"
    );
    assert!(server
        .app
        .metrics
        .render()
        .contains("scope=\"concurrency\""));
    server.stop().await;
}

#[tokio::test]
async fn readyz_answers_a_generic_503_and_logs_the_database_error() {
    let Some(server) = start_with(&[]).await else {
        return;
    };
    let http = client();
    assert_eq!(
        http.get(format!("{}/readyz", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    server.app.pool.close();
    let response = http
        .get(format!("{}/readyz", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"error": "the database is unavailable"}),
        "{body}"
    );
    let logged = aster_srv::log::captured()
        .into_iter()
        .any(|line| line.contains("readiness check failed") && line.contains("\"error\""));
    assert!(logged, "the detail goes to the log");
    server.stop().await;
}

#[tokio::test]
async fn metrics_are_served_on_the_main_port_to_direct_callers_only() {
    let Some(server) = start_with(&[("ASTER_SRV_API_ADDR", "127.0.0.1:0")]).await else {
        return;
    };
    let http = client();
    let direct = http
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(direct.status(), 200);
    assert!(direct.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    let text = direct.text().await.unwrap();
    assert!(text.contains("# TYPE aster_srv_auth_failures_total counter"));
    assert!(text.contains("aster_srv_person_tokens_issued_total"));
    // Through the load balancer (X-Forwarded-For) or IAP: hidden.
    for (name, value) in [
        ("x-forwarded-for", "203.0.113.9, 34.8.0.1"),
        ("x-goog-iap-jwt-assertion", "anything"),
    ] {
        let hidden = http
            .get(format!("{}/metrics", server.url))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(hidden.status(), 404, "{name}");
    }
    // Never on the API port.
    let api = http
        .get(format!("{}/metrics", server.api_url.as_ref().unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), 404);
    server.stop().await;
}

fn payload(token: &str) -> Value {
    serde_json::from_slice(&BASE64URL.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap()
}

#[tokio::test]
async fn auth_failures_are_audited_and_counted_and_no_token_reaches_a_log_or_an_error_body() {
    let Some(server) = start_with(&[]).await else {
        return;
    };
    let url = server.url.clone();
    let http = client();
    let who = "audit-ada@example.com";
    let subject = iap_subject(who);
    let key =
        aster_srv::config::decode_key_secret(common::CLI_TOKEN_KEYS.split_once(':').unwrap().1)
            .unwrap();
    let sign = |kid: &str, key: &[u8], change: &dyn Fn(&mut Value)| {
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({
            "iss": common::PUBLIC_URL, "aud": common::PUBLIC_URL, "sub": subject, "email": who,
            "scope": "cache:read", "iat": now, "nbf": now, "exp": now + 600,
            "jti": "leaky-jti-0001",
        });
        change(&mut claims);
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some(kid.into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(key),
        )
        .unwrap()
    };
    let past = |c: &mut Value| c["exp"] = json!(chrono::Utc::now().timestamp() - 3600);
    let forged = sign("t1", b"not-the-server-key-not-the-server-key", &|_| {});
    let expired = sign("t1", &key, &past);
    let unknown_kid = sign("secret-kid-xyz", &key, &|_| {});
    let wrong_audience_ci = common::token_with_claims(REPO, json!({"aud": "somewhere-else-0042"}));
    let valid_person = sign("t1", &key, &|_| {});
    let plain = "super-secret-static-looking-value-123";
    let forged_iap =
        "eyJhbGciOiJFUzI1NiIsImtpZCI6ImlhcCJ9.eyJzZWNyZXQiOiJsZWFrLW1lIn0.c2lnbmF0dXJlLWxlYWstbWU";
    let wrong_aud_iap = common::iap_assertion_for(who, "/projects/999/global/backendServices/1");

    let mut bodies = String::new();
    for bearer in [&forged, &expired, &unknown_kid, &wrong_audience_ci] {
        let response = lookup(&url, &format!("Bearer {bearer}")).await;
        assert_eq!(response.status(), 401);
        bodies.push_str(&response.text().await.unwrap());
    }
    let response = lookup(&url, &format!("Bearer {plain}")).await;
    assert_eq!(response.status(), 401);
    bodies.push_str(&response.text().await.unwrap());
    // A valid person token on a CI-only route: refused, audited by subject.
    let response = http
        .post(format!("{url}/v1/test-results/runs"))
        .header("authorization", format!("Bearer {valid_person}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    bodies.push_str(&response.text().await.unwrap());
    // IAP failures at /cli-auth.
    for assertion in [forged_iap, wrong_aud_iap.as_str()] {
        let response = http
            .get(format!(
                "{url}/cli-auth?port=40123&user_code={USER_CODE}&challenge={}&state=s",
                pkce().1
            ))
            .header("x-goog-iap-jwt-assertion", assertion)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        bodies.push_str(&response.text().await.unwrap());
    }
    // Exchange failures: malformed, unknown code, and a wrong verifier
    // for a real code (whose subject is then known).
    bodies.push_str(&bad_exchange(&url, None).await.text().await.unwrap());
    let (status, _) = redeem(&url, "short", "short").await;
    assert_eq!(status, 400);
    let (code, real_verifier) = browser_login(&url, &iap_assertion(who)).await;
    let (status, reply) = redeem(&url, &code, &pkce().0).await;
    assert_eq!((status, reply), (400, json!({"error": "invalid_grant"})));
    // A good sign-in, whose token must not be logged either.
    let (code, verifier) = browser_login(&url, &iap_assertion(who)).await;
    let (status, reply) = redeem(&url, &code, &verifier).await;
    assert_eq!(status, 200);
    let issued = reply["token"].as_str().unwrap().to_string();
    aster_srv::audit::token_revoke(&subject, "operator");

    // No secret appears in any log line or error body.
    let log = aster_srv::log::captured().join("\n");
    let secrets = [
        (&forged, "forged"),
        (&expired, "expired"),
        (&unknown_kid, "unknown kid"),
        (&wrong_audience_ci, "ci token"),
        (&valid_person, "person token"),
        (&issued, "issued token"),
    ];
    let mut pieces: Vec<(String, &str)> = vec![
        (plain.into(), "plain bearer"),
        (forged_iap.into(), "forged iap"),
        (wrong_aud_iap.clone(), "iap assertion"),
        (code.clone(), "code"),
        (real_verifier.clone(), "verifier"),
        (verifier.clone(), "verifier 2"),
        ("secret-kid-xyz".into(), "caller's kid"),
        ("leaky-jti-0001".into(), "jti"),
    ];
    for (token, what) in secrets {
        pieces.push((token.clone(), what));
        for segment in token.split('.') {
            if segment.len() >= 16 {
                pieces.push((segment.to_string(), what));
            }
        }
    }
    for segment in forged_iap.split('.').chain(wrong_aud_iap.split('.')) {
        pieces.push((segment.to_string(), "iap segment"));
    }
    for (piece, what) in &pieces {
        assert!(!log.contains(piece.as_str()), "{what} reached the log");
        assert!(
            !bodies.contains(piece.as_str()),
            "{what} reached an error body"
        );
    }
    assert!(!log.contains("eyJ"), "no JWT-shaped text in any log line");
    assert!(!payload(&issued).is_null());

    // The audit lines.
    let lines: Vec<Value> = aster_srv::log::captured()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["audit"] == true)
        .collect();
    let find =
        |event: &str, f: &dyn Fn(&Value) -> bool| lines.iter().any(|l| l["event"] == event && f(l));
    for reason in [
        "bad_signature",
        "expired_token",
        "invalid_token",
        "missing_token",
    ] {
        let seen = find("auth_failure", &|l| {
            l["reason"] == reason && l["route"] == "/v1/cache" && l["client_ip"] == "127.0.0.1"
        });
        // `missing_token` is not exercised above; the other three are.
        assert!(seen || reason == "missing_token", "auth_failure {reason}");
    }
    assert!(find("auth_failure", &|l| l["reason"]
        == "person_token_not_allowed"
        && l["subject"] == subject.as_str()
        && l["route"] == "/v1/test-results/runs"));
    assert!(find("auth_failure", &|l| l["reason"] == "iap_invalid"
        && l["route"] == "/cli-auth"));
    assert!(find("token_mint", &|l| l["subject"] == subject.as_str()
        && l["email"] == who));
    assert!(find("token_exchange", &|l| l["outcome"] == "success"
        && l["subject"] == subject.as_str()));
    assert!(find("token_exchange", &|l| l["outcome"] == "failure"
        && l["reason"] == "unknown_or_expired_code"
        && l["route"] == "/v1/auth/token"));
    assert!(find("token_exchange", &|l| l["outcome"] == "failure"
        && l["reason"] == "malformed_request"));
    assert!(find("token_exchange", &|l| l["outcome"] == "failure"
        && l["reason"] == "pkce_mismatch"
        && l["subject"] == subject.as_str()));
    assert!(find("token_revoke", &|l| l["subject"] == subject.as_str()
        && l["by"] == "operator"));

    // The counters.
    let metrics = server.app.metrics.render();
    for expected in [
        "aster_srv_auth_failures_total{route=\"/v1/cache\",reason=\"bad_signature\"} 1",
        "aster_srv_auth_failures_total{route=\"/v1/cache\",reason=\"expired_token\"} 1",
        "aster_srv_auth_failures_total{route=\"/v1/test-results/runs\",reason=\"person_token_not_allowed\"} 1",
        "aster_srv_auth_failures_total{route=\"/cli-auth\",reason=\"iap_invalid\"} 2",
        "aster_srv_auth_failures_total{route=\"/v1/auth/token\",reason=\"pkce_mismatch\"} 1",
        "aster_srv_person_tokens_issued_total 1",
    ] {
        assert!(metrics.contains(expected), "{expected}\n{metrics}");
    }
    server.stop().await;
}

#[tokio::test]
async fn sign_in_binds_the_stable_iap_subject_not_the_email() {
    let Some(server) = start_with(&[]).await else {
        return;
    };
    let url = server.url.clone();
    let sign_in = |assertion: String| {
        let url = url.clone();
        async move {
            let (code, verifier) = browser_login(&url, &assertion).await;
            let (status, reply) = redeem(&url, &code, &verifier).await;
            assert_eq!(status, 200, "{reply}");
            payload(reply["token"].as_str().unwrap())
        }
    };
    let stable = "accounts.google.com:5550001";
    let before = sign_in(iap_assertion_with_subject("old-name@example.com", stable)).await;
    let after = sign_in(iap_assertion_with_subject("new-name@example.com", stable)).await;
    assert_eq!(before["sub"], stable);
    assert_eq!(after["sub"], stable, "a rename keeps the subject");
    assert_eq!(before["email"], "old-name@example.com");
    assert_eq!(after["email"], "new-name@example.com");
    let other = sign_in(iap_assertion_with_subject(
        "old-name@example.com",
        "accounts.google.com:5550002",
    ))
    .await;
    assert_ne!(
        other["sub"], before["sub"],
        "a reused email is another subject"
    );
    assert_eq!(before["aud"], common::PUBLIC_URL);
    assert_eq!(before["iss"], common::PUBLIC_URL);

    // An assertion with no `sub` cannot sign anyone in.
    let nameless = common::iap_assertion_without_subject("ada@example.com");
    let got = client()
        .get(format!(
            "{url}/cli-auth?port=40123&user_code={USER_CODE}&challenge={}&state=s",
            pkce().1
        ))
        .header("x-goog-iap-jwt-assertion", &nameless)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 401);

    // A code with no subject on its row (minted before the column existed)
    // is refused at exchange.
    let (verifier, challenge) = pkce();
    let code = BASE64URL.encode([9u8; 32]);
    server
        .client()
        .await
        .execute(
            "INSERT INTO auth.cli_codes (code_hash, email, subject, challenge, port, expires_at) \
             VALUES ($1, 'ada@example.com', NULL, $2, 40123, now() + interval '60 seconds')",
            &[&Sha256::digest(code.as_bytes()).as_slice(), &challenge],
        )
        .await
        .unwrap();
    let (status, reply) = redeem(&url, &code, &verifier).await;
    assert_eq!((status, reply), (400, json!({"error": "invalid_grant"})));
    assert!(server
        .app
        .metrics
        .render()
        .contains("reason=\"code_without_subject\""));
    server.stop().await;
}

#[tokio::test]
async fn https_deployments_send_hsts_on_every_response_and_auth_responses_are_never_cached() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_API_ADDR", "127.0.0.1:0"),
        ("ASTER_SRV_MAX_BODY_BYTES", "1024"),
    ])
    .await
    else {
        return;
    };
    assert!(common::PUBLIC_URL.starts_with("https://"));
    let ada = iap_assertion("ada@example.com");
    let api = server.api_url.clone().unwrap();
    let (code, verifier) = browser_login(&server.url, &ada).await;
    let http = client();
    let big = "x".repeat(2000);
    let mut responses = vec![
        (
            "token success",
            http.post(format!("{api}/v1/auth/token"))
                .json(&json!({"code": code, "verifier": verifier, "port": 40123, "user_code": USER_CODE}))
                .send()
                .await
                .unwrap(),
        ),
        ("token failure", bad_exchange(&api, None).await),
        (
            "cache without a token",
            lookup(&server.url, "Bearer nope").await,
        ),
        (
            "api 404",
            http.get(format!("{api}/ui")).send().await.unwrap(),
        ),
        (
            "main 404",
            http.get(format!("{}/nothing", server.url))
                .send()
                .await
                .unwrap(),
        ),
        (
            "405",
            http.delete(format!("{}/healthz", server.url))
                .send()
                .await
                .unwrap(),
        ),
        (
            "413",
            http.post(format!("{}/v1/cache/lookup", server.url))
                .body(big)
                .send()
                .await
                .unwrap(),
        ),
        (
            "health",
            http.get(format!("{}/healthz", server.url))
                .send()
                .await
                .unwrap(),
        ),
        (
            "api health",
            http.get(format!("{api}/v1/healthz")).send().await.unwrap(),
        ),
        (
            "login page",
            http.get(format!(
                "{}/cli-auth?port=40123&user_code={USER_CODE}&challenge={}&state=s",
                server.url,
                pkce().1
            ))
            .header("x-goog-iap-jwt-assertion", &ada)
            .send()
            .await
            .unwrap(),
        ),
        (
            "login without IAP",
            http.get(format!(
                "{}/cli-auth?port=40123&user_code={USER_CODE}&challenge={}&state=s",
                server.url,
                pkce().1
            ))
            .send()
            .await
            .unwrap(),
        ),
        (
            "dashboard",
            http.get(format!("{}/ui", server.url))
                .header("x-goog-iap-jwt-assertion", &ada)
                .send()
                .await
                .unwrap(),
        ),
    ];
    // A 429 too.
    let limited = start_with(&[("ASTER_SRV_RATE_API_IP_PER_MIN", "1")])
        .await
        .unwrap();
    lookup(&limited.url, "Bearer x").await;
    responses.push(("429", lookup(&limited.url, "Bearer x").await));
    for (what, response) in &responses {
        let headers = response.headers();
        assert_eq!(headers["strict-transport-security"], HSTS, "{what}");
        assert_eq!(headers["cache-control"], "no-store", "{what}");
        assert_eq!(headers["x-content-type-options"], "nosniff", "{what}");
    }
    assert_eq!(responses[0].1.status(), 200);
    limited.stop().await;
    server.stop().await;

    // A development server on plain http does not claim HSTS.
    let Some(dev) = start_with(&[
        ("ASTER_SRV_PUBLIC_URL", "http://127.0.0.1:1"),
        ("ASTER_SRV_ALLOW_HTTP_PUBLIC_URL", "1"),
    ])
    .await
    else {
        return;
    };
    let response = client()
        .get(format!("{}/healthz", dev.url))
        .send()
        .await
        .unwrap();
    assert!(!response.headers().contains_key("strict-transport-security"));
    assert_eq!(response.headers()["cache-control"], "no-store");
    dev.stop().await;
}

// ------------------------------------------------- N3: unauthenticated slowloris

/// A POST that declares 100 body bytes and sends one, from the client the
/// load balancer would report as `client` (`X-Forwarded-For` is
/// `<spoofed>, <client>, <balancer>`). The stream stays open, holding the
/// request in flight.
async fn stalled_post(url: &str, path: &str, client: &str) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(url.trim_start_matches("http://"))
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: x\r\nx-forwarded-for: 6.6.6.6, {client}, 34.8.0.1\r\n\
                 Content-Length: 100\r\n\r\n{{"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream
}

/// N3: an unauthenticated body gets its own short deadline, well under the
/// global body timeout that authenticated uploads keep.
#[tokio::test]
async fn unauthenticated_bodies_time_out_in_seconds_while_authenticated_ones_keep_the_long_timeout()
{
    let Some(server) = start_with(&[
        ("ASTER_SRV_API_ADDR", "127.0.0.1:0"),
        ("ASTER_SRV_UNAUTH_BODY_TIMEOUT_SECS", "1"),
        ("ASTER_SRV_BODY_TIMEOUT_SECS", "30"),
    ])
    .await
    else {
        return;
    };
    for url in [&server.url, server.api_url.as_ref().unwrap()] {
        for path in ["/v1/auth/token", "/cli-auth"] {
            let stalled =
                format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{{");
            let started = Instant::now();
            let reply = raw(url, stalled.as_bytes(), Duration::from_secs(10)).await;
            if path == "/cli-auth" && url != &server.url {
                // Not served on the API port at all.
                assert_eq!(status_of(&reply), Some(404), "{reply}");
                continue;
            }
            assert_eq!(status_of(&reply), Some(408), "{url}{path}: {reply}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{url}{path}: {:?}",
                started.elapsed()
            );
        }
    }
    // A CI upload with a valid token is still waited for: no reply in 3s.
    let stalled = format!(
        "POST /v1/test-results/runs HTTP/1.1\r\nHost: x\r\nauthorization: {}\r\n\
         Content-Length: 100\r\n\r\n{{",
        ci_bearer()
    );
    let started = Instant::now();
    let reply = raw(&server.url, stalled.as_bytes(), Duration::from_secs(3)).await;
    assert_eq!(reply, "", "an authenticated body keeps the long timeout");
    assert!(started.elapsed() >= Duration::from_secs(3));
    server.stop().await;
}

/// N3: `header_read_timeout` also bounds an idle keep-alive connection: after
/// a completed request the connection is closed once no next request's
/// headers arrive in time.
#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed_after_the_header_timeout() {
    let Some(server) = start_with(&[("ASTER_SRV_HEADER_TIMEOUT_SECS", "1")]).await else {
        return;
    };
    let mut stream = tokio::net::TcpStream::connect(server.url.trim_start_matches("http://"))
        .await
        .unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buffer = [0u8; 2048];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("answered")
        .unwrap();
    assert!(String::from_utf8_lossy(&buffer[..n]).starts_with("HTTP/1.1 200"));
    // Keep-alive: the connection stays open for the next request, then goes.
    let started = Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer))
        .await
        .expect("an idle connection must not stay open");
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "idle for {:?}",
        started.elapsed()
    );
    server.stop().await;
}

/// N3: behind the balancer one client IP, as `client_ip` resolves it, holds
/// at most `ASTER_SRV_MAX_CONNECTIONS_PER_IP` requests open; a spoofed
/// `X-Forwarded-For` prefix does not make a second client of it, and another
/// client is unaffected.
#[tokio::test]
async fn one_client_ip_cannot_hold_more_than_its_share_of_requests_open() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_MAX_CONNECTIONS_PER_IP", "2"),
        ("ASTER_SRV_UNAUTH_BODY_TIMEOUT_SECS", "30"),
        ("ASTER_SRV_BODY_TIMEOUT_SECS", "30"),
    ])
    .await
    else {
        return;
    };
    let url = &server.url;
    let first = stalled_post(url, "/v1/auth/token", "203.0.113.7").await;
    let _second = stalled_post(url, "/v1/auth/token", "203.0.113.7").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The third from the same client, whatever it prefixes, is turned away.
    let third = format!(
        "GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         x-forwarded-for: 1.2.3.4, 203.0.113.7, 34.8.0.1\r\n\r\n"
    );
    let reply = raw(url, third.as_bytes(), Duration::from_secs(5)).await;
    assert_eq!(status_of(&reply), Some(429), "{reply}");
    assert!(reply.to_lowercase().contains("retry-after"), "{reply}");
    // Another client address is served.
    let other = "GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         x-forwarded-for: 1.2.3.4, 203.0.113.8, 34.8.0.1\r\n\r\n";
    let reply = raw(url, other.as_bytes(), Duration::from_secs(5)).await;
    assert_eq!(status_of(&reply), Some(200), "{reply}");
    // The first slot frees when its request ends.
    drop(first);
    let mut status = None;
    for _ in 0..50 {
        let reply = raw(url, third.as_bytes(), Duration::from_secs(5)).await;
        status = status_of(&reply);
        if status == Some(200) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status, Some(200));
    let metrics = server.app.metrics.render();
    assert!(metrics.contains("scope=\"ip_concurrency\""), "{metrics}");
    server.stop().await;
}

/// N3: with no trusted proxy the TCP peer is the client, so its open
/// connections are capped at accept: the extra one is closed unserved.
#[tokio::test]
async fn without_a_proxy_one_peer_cannot_hold_more_connections_than_its_share() {
    let Some(server) = start_with(&[
        ("ASTER_SRV_TRUSTED_PROXY_HOPS", "0"),
        ("ASTER_SRV_MAX_CONNECTIONS_PER_IP", "2"),
        ("ASTER_SRV_HEADER_TIMEOUT_SECS", "60"),
    ])
    .await
    else {
        return;
    };
    let address = server.url.trim_start_matches("http://").to_string();
    let first = tokio::net::TcpStream::connect(&address).await.unwrap();
    let _second = tokio::net::TcpStream::connect(&address).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reply = raw(
        &server.url,
        b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        reply, "",
        "a third connection from the peer is closed unserved"
    );
    drop(first);
    let mut served = false;
    for _ in 0..50 {
        let reply = raw(
            &server.url,
            b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            Duration::from_secs(5),
        )
        .await;
        if status_of(&reply) == Some(200) {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(served, "a slot frees when a connection closes");
    server.stop().await;
}
