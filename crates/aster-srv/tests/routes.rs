//! The route table's guarantees, against a real server and Postgres.
//!
//! Every test walks `aster_srv::routes::ROUTES`, so a route added to the
//! table is covered without editing a test, and a route that skips the auth
//! gate fails them.

mod common;

use std::sync::Arc;

use aster_srv::cache::{BlobStore, FixedSigner, Signer};
use aster_srv::routes::{Auth, Route, ROUTES};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use common::{iap_assertion, person_bearer, token_with_claims, Server, REPO};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};

const CI_WORKFLOW: &str = "ArchAstro/firstlanding/.github/workflows/ci.yml@refs/heads/main";
const OTHER_KEY: &[u8] = include_bytes!("fixtures/test-other-key.pem");
const PUBLIC_KEY: &[u8] = include_bytes!("fixtures/test-oidc-key.pub.pem");
const PRIVATE_KEY: &[u8] = include_bytes!("fixtures/test-oidc-key.pem");
const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

async fn start() -> Option<Server> {
    let signer = Arc::new(FixedSigner {
        email: "cache-test@example.iam.gserviceaccount.com".into(),
        signature: b"test-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory("aster-cache-test", signer as Arc<dyn Signer>);
    common::start_with(
        &[
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
            (
                "ASTER_SRV_CACHE_WRITER_WORKFLOWS",
                "ArchAstro/firstlanding/.github/workflows/ci.yml@",
            ),
            ("ASTER_SRV_REPOSITORY_OWNER_IDS", "ArchAstro/*=4242"),
        ],
        Some(store),
    )
    .await
}

/// A route's pattern with each `:name` filled in.
fn path_of(route: &Route) -> String {
    route
        .pattern
        .split('/')
        .map(|segment| {
            if segment.starts_with(':') {
                "x"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Every route under `/v1/` that is not the code exchange, plus a path the
/// table does not list, which the gate must also refuse.
fn v1_routes() -> Vec<(String, String, Auth)> {
    let mut routes: Vec<(String, String, Auth)> = ROUTES
        .iter()
        .filter(|route| route.pattern.starts_with("/v1/") && route.auth != Auth::Public)
        .map(|route| (route.method.to_string(), path_of(route), route.auth))
        .collect();
    routes.push(("GET".into(), "/v1/not-a-route".into(), Auth::CiOrPerson));
    routes.push(("DELETE".into(), "/v1/cache/lookup".into(), Auth::CiOrPerson));
    routes
}

async fn send(
    server: &Server,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let client = aster_srv::http_client().build().unwrap();
    let url = format!("{}{path}", server.url);
    let mut request = match method {
        "POST" => client.post(url).body("{}"),
        "PUT" => client.put(url).body("{}"),
        "DELETE" => client.delete(url),
        _ => client.get(url),
    };
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

fn jwt(header: Header, key: &EncodingKey, claims: Value) -> String {
    encode(&header, &claims, key).unwrap()
}

fn oidc_claims() -> Value {
    json!({
        "iss": "https://token.actions.githubusercontent.com", "aud": "aster-srv",
        "exp": chrono::Utc::now().timestamp() + 300, "repository": REPO,
        "ref": "refs/heads/main", "event_name": "push",
        "job_workflow_ref": CI_WORKFLOW, "workflow_ref": CI_WORKFLOW,
    })
}

fn rs256(key: &[u8], claims: Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    jwt(header, &EncodingKey::from_rsa_pem(key).unwrap(), claims)
}

/// Tokens that must never open a route: `(what, header value)`.
fn bad_credentials() -> Vec<(&'static str, Option<String>)> {
    let with = |change: &dyn Fn(&mut Value)| {
        let mut claims = oidc_claims();
        change(&mut claims);
        claims
    };
    let bearer = |token: String| Some(format!("Bearer {token}"));
    let hs256 = |kid: &str, secret: &[u8], claims: Value| {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.into());
        jwt(header, &EncodingKey::from_secret(secret), claims)
    };
    let person_claims = |change: &dyn Fn(&mut Value)| {
        let mut claims = json!({
            "iss": common::PUBLIC_URL, "aud": "aster-cli", "sub": "ada@example.com",
            "scope": "read", "iat": chrono::Utc::now().timestamp(),
            "exp": chrono::Utc::now().timestamp() + 300, "jti": "j",
        });
        change(&mut claims);
        claims
    };
    let none = format!(
        "{}.{}.",
        BASE64URL.encode(r#"{"alg":"none","typ":"JWT","kid":"test"}"#),
        BASE64URL.encode(oidc_claims().to_string()),
    );
    vec![
        ("no credentials", None),
        ("a garbage token", bearer("garbage-token-0123456789".into())),
        ("an empty bearer", Some("Bearer ".into())),
        ("another scheme", Some("Basic YTpi".into())),
        (
            "an expired token",
            bearer(rs256(
                PRIVATE_KEY,
                with(&|c| c["exp"] = json!(chrono::Utc::now().timestamp() - 3600)),
            )),
        ),
        (
            "a token signed with the wrong key",
            bearer(rs256(OTHER_KEY, oidc_claims())),
        ),
        (
            "a token that is not valid yet (nbf)",
            bearer(rs256(
                PRIVATE_KEY,
                with(&|c| c["nbf"] = json!(chrono::Utc::now().timestamp() + 3600)),
            )),
        ),
        ("alg=none", bearer(none)),
        (
            "an HS256 token keyed with the RSA public key (algorithm confusion)",
            bearer(hs256("test", PUBLIC_KEY, oidc_claims())),
        ),
        (
            "an HS256 token keyed with the RSA private key",
            bearer(hs256("test", PRIVATE_KEY, oidc_claims())),
        ),
        (
            "an HS256 token for the person key id signed with the RSA public key",
            bearer(hs256("t1", PUBLIC_KEY, person_claims(&|_| {}))),
        ),
        (
            "an OIDC token for another audience",
            bearer(rs256(
                PRIVATE_KEY,
                with(&|c| c["aud"] = json!("someone-else")),
            )),
        ),
        (
            "an OIDC token from another issuer",
            bearer(rs256(
                PRIVATE_KEY,
                with(&|c| c["iss"] = json!("https://evil.example")),
            )),
        ),
        (
            "a person token for the wrong audience",
            bearer(hs256(
                "t1",
                common::CLI_TOKEN_KEYS.split_once(':').unwrap().1.as_bytes(),
                person_claims(&|c| c["aud"] = json!("aster-srv")),
            )),
        ),
        (
            "a person token from the wrong issuer",
            bearer(hs256(
                "t1",
                common::CLI_TOKEN_KEYS.split_once(':').unwrap().1.as_bytes(),
                person_claims(&|c| c["iss"] = json!("https://evil.example")),
            )),
        ),
    ]
}

#[tokio::test]
async fn every_v1_route_answers_401_to_every_kind_of_bad_credential() {
    let Some(server) = start().await else {
        return;
    };
    // IAP in front of the server is not a credential on /v1: a perfectly
    // signed assertion, or headers claiming IAP's identity, open nothing.
    let forged: Vec<(&str, Vec<(&str, String)>)> = vec![
        (
            "a valid IAP assertion and no token",
            vec![("x-goog-iap-jwt-assertion", iap_assertion("ada@example.com"))],
        ),
        (
            "forged x-goog-iap headers",
            vec![
                (
                    "x-goog-authenticated-user-email",
                    "accounts.google.com:ada@example.com".into(),
                ),
                ("x-goog-iap-jwt-assertion", "forged.iap.assertion".into()),
                (
                    "x-goog-authenticated-user-id",
                    "accounts.google.com:1".into(),
                ),
            ],
        ),
    ];
    for (method, path, _) in v1_routes() {
        let path = format!("{path}?repo=ArchAstro%2Ffirstlanding");
        for (what, authorization) in bad_credentials() {
            let headers: Vec<(&str, &str)> = authorization
                .as_deref()
                .map(|value| vec![("authorization", value)])
                .unwrap_or_default();
            let (status, body) = send(&server, &method, &path, &headers).await;
            assert_eq!(status, 401, "{method} {path} with {what}: {body}");
            // Whatever is wrong with it, the reply says only that. The
            // reason is logged, not sent.
            if authorization.is_some() {
                assert!(
                    body == r#"{"error":"the token is not valid"}"#
                        || body == r#"{"error":"send a bearer token"}"#,
                    "{method} {path} with {what}: {body}"
                );
            }
            // The token is never echoed back.
            if let Some(value) = &authorization {
                let token = value.rsplit(' ').next().unwrap_or_default();
                assert!(
                    token.is_empty() || !body.contains(token),
                    "{method} {path} with {what} echoed the token: {body}"
                );
            }
        }
        for (what, headers) in &forged {
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
            let (status, body) = send(&server, &method, &path, &headers).await;
            assert_eq!(status, 401, "{method} {path} with {what}: {body}");
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn the_code_exchange_is_the_only_v1_route_without_a_token_and_never_says_200_without_a_code()
{
    let Some(server) = start().await else {
        return;
    };
    let public: Vec<&str> = ROUTES
        .iter()
        .filter(|route| route.pattern.starts_with("/v1/") && route.auth == Auth::Public)
        .map(|route| route.pattern)
        .collect();
    assert_eq!(public, ["/v1/auth/token"]);
    let client = aster_srv::http_client().build().unwrap();
    for body in [
        "",
        "grant_type=authorization_code&code=nope&code_verifier=nope",
        "grant_type=authorization_code&code=&code_verifier=",
        "code=nope",
    ] {
        let response = client
            .post(format!("{}/v1/auth/token", server.url))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        assert_ne!(status, 200, "{body:?}");
        assert!(status == 400, "{body:?}: {status}");
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["error"], "invalid_grant", "{body:?}");
    }
    server.stop().await;
}

/// The reads a person's token may make: the whole allowlist, spelled out so
/// a new route classed `CiOrPerson` is a decision made here too.
const PERSON_READS: [(&str, &str); 10] = [
    ("GET", "/v1/test-results/durations"),
    ("GET", "/v1/test-results/tests"),
    ("GET", "/v1/test-results/tests/:test_id"),
    ("GET", "/v1/test-results/failures"),
    ("GET", "/v1/test-results/quarantine"),
    ("GET", "/v1/test-results/clusters"),
    ("GET", "/v1/test-results/runs/:run_id/failures"),
    ("GET", "/v1/cache/summary"),
    ("GET", "/v1/cache/entries"),
    ("POST", "/v1/cache/lookup"),
];

#[tokio::test]
async fn a_person_reaches_only_the_read_allowlist_and_every_other_route_is_403() {
    let Some(server) = start().await else {
        return;
    };
    let allowed: Vec<(&str, &str)> = ROUTES
        .iter()
        .filter(|route| route.auth == Auth::CiOrPerson)
        .map(|route| (route.method, route.pattern))
        .collect();
    let expected: Vec<(&str, &str)> = PERSON_READS.to_vec();
    assert_eq!(allowed, expected, "the person allowlist changed");

    let person = person_bearer(&server, "ada@example.com");
    let mut checked = 0;
    for route in ROUTES.iter().filter(|r| r.pattern.starts_with("/v1/")) {
        if route.auth == Auth::Public {
            continue;
        }
        let path = path_of(route);
        let post_body = |repo: &str| json!({"repo": repo, "keys": ["k"]}).to_string();
        for repo in [REPO, "evil/x"] {
            let url = format!("{}{path}?repo={}", server.url, repo.replace('/', "%2F"));
            let client = aster_srv::http_client().build().unwrap();
            let request = match route.method {
                "POST" => client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(post_body(repo)),
                _ => client.get(&url),
            };
            let status = request
                .header("authorization", &person)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16();
            let what = format!("{} {} for {repo}", route.method, route.pattern);
            match (route.auth, repo) {
                (Auth::CiOrPerson, REPO) => {
                    assert!(![401, 403].contains(&status), "{what}: {status}");
                }
                _ => assert_eq!(status, 403, "{what}"),
            }
        }
        checked += 1;
    }
    assert!(checked >= 13, "the walk covered {checked} routes");
    server.stop().await;
}

async fn write_status(server: &Server, token: &str, path: &str) -> u16 {
    let manifest = base64::engine::general_purpose::STANDARD
        .encode(json!({"schema": 1, "key": "k", "target": "//a:b", "outputs": []}).to_string());
    let body = match path {
        "/v1/cache/upload" => json!({"repo": REPO, "digests": [DIGEST]}).to_string(),
        "/v1/cache/commit" => json!({"repo": REPO, "key": "k", "manifest": manifest}).to_string(),
        _ => String::new(),
    };
    aster_srv::http_client()
        .build()
        .unwrap()
        .post(format!("{}{path}", server.url))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn cache_writes_need_a_push_to_main_from_the_ci_workflow_in_an_allowed_repository() {
    let Some(server) = start().await else {
        return;
    };
    let base = json!({
        "ref": "refs/heads/main", "event_name": "push",
        "job_workflow_ref": CI_WORKFLOW, "workflow_ref": CI_WORKFLOW,
        "repository_owner_id": "4242",
    });
    let variant = |change: &Value| {
        let mut claims = base.clone();
        for (name, value) in change.as_object().unwrap() {
            claims[name] = value.clone();
        }
        token_with_claims(REPO, claims)
    };
    assert_eq!(
        write_status(&server, &variant(&json!({})), "/v1/cache/upload").await,
        200,
        "control: a push to main from ci.yml writes"
    );
    let evil = "ArchAstro/firstlanding/.github/workflows/evil.yml@refs/heads/main";
    let cases: Vec<(&str, Value)> = vec![
        (
            "a pull request",
            json!({"ref": "refs/pull/7/merge", "event_name": "pull_request"}),
        ),
        (
            "a pull request that claims main",
            json!({"event_name": "pull_request"}),
        ),
        (
            "pull_request_target",
            json!({"event_name": "pull_request_target"}),
        ),
        ("a feature branch", json!({"ref": "refs/heads/feature"})),
        (
            "a merge queue",
            json!({"ref": "refs/heads/gh-readonly-queue/main/pr-7-abc", "event_name": "merge_group"}),
        ),
        (
            "workflow_dispatch",
            json!({"event_name": "workflow_dispatch"}),
        ),
        ("a schedule", json!({"event_name": "schedule"})),
        ("no event_name", json!({"event_name": null})),
        (
            "another workflow",
            json!({"job_workflow_ref": evil, "workflow_ref": evil}),
        ),
        (
            "a no workflow claim",
            json!({"job_workflow_ref": null, "workflow_ref": null}),
        ),
    ];
    for (what, change) in cases {
        let token = variant(&change);
        for path in ["/v1/cache/upload", "/v1/cache/commit"] {
            let status = write_status(&server, &token, path).await;
            assert_eq!(status, 403, "{what} on {path}");
        }
    }
    // A repository off the allowlist is refused by the verifier itself.
    let fork = token_with_claims("evil/fork", base.clone());
    assert_eq!(write_status(&server, &fork, "/v1/cache/upload").await, 403);
    server.stop().await;
}

#[tokio::test]
async fn test_results_uploads_need_the_ci_workflow_and_refuse_pull_request_target() {
    let Some(server) = start().await else {
        return;
    };
    let upload = |extra: Value| {
        let token = token_with_claims(REPO, extra);
        let server = &server;
        async move { write_status(server, &token, "/v1/test-results/runs").await }
    };
    let base = json!({"ref": "refs/heads/main", "job_workflow_ref": CI_WORKFLOW, "repository_owner_id": "4242"});
    let merged = |change: Value| {
        let mut claims = base.clone();
        for (name, value) in change.as_object().unwrap() {
            claims[name] = value.clone();
        }
        claims
    };
    // Control: past the gate (an empty upload is the handler's 400, not a 403).
    assert_eq!(upload(base.clone()).await, 400);
    // Pull requests upload: a retry that passes there is the flake signal.
    assert_eq!(
        upload(merged(
            json!({"ref": "refs/pull/7/merge", "event_name": "pull_request"})
        ))
        .await,
        400
    );
    for (what, change) in [
        (
            "pull_request_target",
            json!({"event_name": "pull_request_target"}),
        ),
        (
            "another workflow",
            json!({"job_workflow_ref": "ArchAstro/firstlanding/.github/workflows/evil.yml@refs/heads/main"}),
        ),
        ("no workflow claim", json!({"job_workflow_ref": null})),
    ] {
        assert_eq!(upload(merged(change)).await, 403, "{what}");
    }
    let fork = token_with_claims("evil/fork", base.clone());
    assert_eq!(
        write_status(&server, &fork, "/v1/test-results/runs").await,
        403
    );
    server.stop().await;
}

#[tokio::test]
async fn a_request_is_authenticated_before_its_body_is_read_and_capped_after() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(server) = start().await else {
        return;
    };
    let address = server.url.trim_start_matches("http://").to_string();
    // A declared 100 MB body that is never sent: if the server waited for it
    // before checking the token, this would hang until the read timed out.
    for path in [
        "/v1/cache/commit",
        "/v1/cache/upload",
        "/v1/cache/lookup",
        "/v1/test-results/runs",
    ] {
        let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\n\
                     content-length: 100000000\r\n\r\n{{"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reply = Vec::new();
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut buffer = [0u8; 512];
            loop {
                let n = stream.read(&mut buffer).await.unwrap();
                reply.extend_from_slice(&buffer[..n]);
                if n == 0 || reply.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        })
        .await;
        assert!(read.is_ok(), "{path}: no reply while the body was unsent");
        // Either refusal needs no body: 413 when the declared length is over
        // the route's cap (checked from the header alone), else 401 from the
        // gate. Never a wait for the body.
        let reply_text = String::from_utf8_lossy(&reply);
        assert!(
            reply_text.starts_with("HTTP/1.1 401") || reply_text.starts_with("HTTP/1.1 413"),
            "{path}: {}",
            String::from_utf8_lossy(&reply)
        );
    }
    // With a valid token, a lookup or upload is capped at 1 MiB and a
    // commit is not.
    let ci = token_with_claims(
        REPO,
        json!({"repository_owner_id": "4242", "job_workflow_ref": CI_WORKFLOW, "ref": "refs/heads/main"}),
    );
    let big = "x".repeat(2 << 20);
    let client = aster_srv::http_client().build().unwrap();
    for (path, expected) in [
        ("/v1/cache/lookup", 413),
        ("/v1/cache/upload", 413),
        ("/v1/cache/commit", 400),
    ] {
        let status = client
            .post(format!("{}{path}", server.url))
            .bearer_auth(&ci)
            .body(big.clone())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(status, expected, "{path}");
    }
    server.stop().await;
}

#[tokio::test]
async fn a_persons_lookup_does_not_count_or_keep_an_entry_alive_but_ci_does() {
    let Some(server) = start().await else {
        return;
    };
    let ci = format!(
        "Bearer {}",
        token_with_claims(
            REPO,
            json!({"repository_owner_id": "4242", "job_workflow_ref": CI_WORKFLOW, "ref": "refs/heads/main"}),
        )
    );
    let manifest = base64::engine::general_purpose::STANDARD
        .encode(json!({"schema": 1, "key": "k", "target": "//a:b", "outputs": []}).to_string());
    let (status, body) = server
        .post_json(
            "/v1/cache/commit",
            &json!({"repo": REPO, "key": "k", "manifest": manifest}),
            Some(&ci),
            None,
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let hits = || async {
        let row = server
            .client()
            .await
            .query_one(
                "SELECT hits, last_hit_at IS NOT NULL FROM cache.manifests WHERE key = 'k'",
                &[],
            )
            .await
            .unwrap();
        (row.get::<_, i64>(0), row.get::<_, bool>(1))
    };
    let person = person_bearer(&server, "ada@example.com");
    let lookup = json!({"repo": REPO, "keys": ["k"]});
    for _ in 0..3 {
        let (status, body) = server
            .post_json("/v1/cache/lookup", &lookup, Some(&person), None)
            .await;
        assert_eq!(status, 200);
        assert!(body["hits"]["k"].is_object(), "the person still reads it");
    }
    assert_eq!(
        hits().await,
        (0, false),
        "a person's lookups leave no trace"
    );
    let (status, _) = server
        .post_json("/v1/cache/lookup", &lookup, Some(&ci), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(hits().await, (1, true), "CI's lookups count");
    server.stop().await;
}

#[tokio::test]
async fn the_signed_blob_stand_in_exists_only_when_the_dev_flag_is_set() {
    let Some(server) = start().await else {
        return;
    };
    let path = format!("/aster-cache-test/cas/{DIGEST}");
    // `start` leaves the flag at its test default, on.
    let (on, _) = send(&server, "PUT", &path, &[]).await;
    assert_ne!(on, 404, "the stand-in answers in the test setup");
    server.stop().await;

    let signer = Arc::new(FixedSigner {
        email: "cache-test@example.iam.gserviceaccount.com".into(),
        signature: b"test-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory("aster-cache-test", signer as Arc<dyn Signer>);
    let Some(server) = common::start_with(
        &[
            ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
            (
                "ASTER_SRV_CACHE_SIGNER",
                "cache-test@example.iam.gserviceaccount.com",
            ),
            ("ASTER_SRV_DEV_BLOB_ROUTE", "0"),
        ],
        Some(store),
    )
    .await
    else {
        return;
    };
    for method in ["PUT", "GET"] {
        let (status, _) = send(&server, method, &path, &[]).await;
        assert_eq!(
            status, 404,
            "{method} without the flag, even on a memory store"
        );
    }
    server.stop().await;
}
