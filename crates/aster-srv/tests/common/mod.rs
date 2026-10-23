//! Shared by the classifier and query tests: a server on a fresh database,
//! tokens, and a way to put runs into the database at chosen times.
//!
//! Set ASTER_SRV_TEST_DATABASE_URL to a Postgres URL whose user may create
//! databases; without it `start` returns `None` and the tests skip.
#![allow(dead_code)]

use std::sync::Arc;

use aster_srv::auth::Verifier;
use aster_srv::iap::IapVerifier;
use aster_srv::raw_store::RawStore;
use aster_srv::{App, Config};
use chrono::{DateTime, Utc};
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use serde_json::{json, Value};

const PRIVATE: &[u8] = include_bytes!("../fixtures/test-oidc-key.pem");
const PUBLIC: &[u8] = include_bytes!("../fixtures/test-oidc-key.pub.pem");
/// The audience dashboard tests configure and sign for.
pub const IAP_AUDIENCE: &str = "/projects/123456/global/backendServices/4242";
pub const REPO: &str = "ArchAstro/firstlanding";
/// What the test server answers to, and how it signs person tokens.
pub const PUBLIC_URL: &str = "https://aster-srv.test";
/// 32 random bytes as base64 (generated once for these tests).
pub const CLI_TOKEN_KEYS: &str = "t1:q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=";

/// A P-256 key pair made for this test process, so no private key is
/// checked in: `(signing key as PKCS#8, public key as an uncompressed point)`.
fn iap_key_pair() -> &'static (Vec<u8>, Vec<u8>) {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    static PAIR: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
    PAIR.get_or_init(|| {
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .expect("a P-256 key is generated");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref())
            .expect("the generated key loads");
        (pkcs8.as_ref().to_vec(), pair.public_key().as_ref().to_vec())
    })
}

pub struct Server {
    pub app: Arc<App>,
    pub url: String,
    /// The `/v1`-only listener, when `ASTER_SRV_API_ADDR` was set.
    pub api_url: Option<String>,
    admin_url: String,
    database: String,
    database_url: String,
    /// The settings it started with, which [`Server::second`] repeats.
    env: Vec<(String, String)>,
    /// False for a [`Server::second`] on another server's database.
    owns_database: bool,
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
}

impl Server {
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true);
        }
        self.app.pool.close();
        if !self.owns_database {
            return;
        }
        let (admin, connection) = tokio_postgres::connect(&self.admin_url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        admin
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.database
            ))
            .await
            .unwrap();
    }

    pub async fn client(&self) -> deadpool_postgres::Client {
        self.app.pool.get().await.unwrap()
    }

    /// `GET path` with a token for `repository` (none for `None`).
    pub async fn get(&self, path: &str, repository: Option<&str>) -> (u16, Value) {
        let mut request = aster_srv::http_client()
            .build()
            .unwrap()
            .get(format!("{}{path}", self.url));
        if let Some(repository) = repository {
            request = request.bearer_auth(token(repository));
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(json!(null)))
    }
}

/// A server on a fresh database, or `None` when no test database is set.
/// `env` adds settings such as `ASTER_SRV_BROKEN_RUNS`.
pub async fn start(env: &[(&str, &str)]) -> Option<Server> {
    start_with(env, None).await
}

/// [`start`] with a chosen cache blob store.
pub async fn start_with(
    env: &[(&str, &str)],
    cache_store: Option<aster_srv::cache::BlobStore>,
) -> Option<Server> {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return None;
    };
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let database = format!(
        "aster_srv_test_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
    );
    let (admin, connection) = tokio_postgres::connect(&admin_url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .await
        .unwrap();
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let database_url = format!("{base}/{database}");

    let (app, url, api_url, shutdown) = launch(&database_url, env, cache_store).await;
    Some(Server {
        app,
        url,
        api_url,
        admin_url,
        database,
        database_url,
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        owns_database: true,
        shutdown: Some(shutdown),
    })
}

/// Build the app on `database_url` and serve it on a free port.
async fn launch(
    database_url: &str,
    env: &[(&str, &str)],
    cache_store: Option<aster_srv::cache::BlobStore>,
) -> (
    Arc<App>,
    String,
    Option<String>,
    tokio::sync::watch::Sender<bool>,
) {
    let has = |name: &str| {
        env.iter()
            .any(|(key, value)| *key == name && !value.is_empty())
    };
    let config = Config::from_env(|name| match name {
        "ASTER_SRV_DATABASE_URL" => Some(database_url.to_string()),
        "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
        other => env
            .iter()
            .find(|(key, _)| *key == other)
            .map(|(_, value)| value.to_string())
            // Person sign-in is on unless a test sets either to "" (unset).
            .or_else(|| match other {
                "ASTER_SRV_PUBLIC_URL" => Some(PUBLIC_URL.into()),
                // Sign-in needs IAP and a domain allowlist to start at all.
                "ASTER_SRV_CLI_TOKEN_KEYS"
                    if has("ASTER_SRV_IAP_AUDIENCE") && has("ASTER_SRV_UI_EMAIL_DOMAINS") =>
                {
                    Some(CLI_TOKEN_KEYS.into())
                }
                // The `/v1` listener refuses to start with the stand-in route,
                // without owner-id pins, or without a writer-workflow list.
                "ASTER_SRV_DEV_BLOB_ROUTE" if !has("ASTER_SRV_API_ADDR") => Some("1".into()),
                "ASTER_SRV_REPOSITORY_OWNER_IDS" if has("ASTER_SRV_API_ADDR") => {
                    Some("ArchAstro/*=4242".into())
                }
                "ASTER_SRV_CACHE_WRITER_WORKFLOWS" if has("ASTER_SRV_API_ADDR") => {
                    Some("ArchAstro/firstlanding/.github/workflows/ci.yml@".into())
                }
                _ => None,
            }),
    })
    .unwrap();
    let verifier =
        Verifier::new(&config).with_key("test", DecodingKey::from_rsa_pem(PUBLIC).unwrap());
    let iap =
        IapVerifier::new(&config).with_key("iap", DecodingKey::from_ec_der(&iap_key_pair().1));
    let app = App::start_with_cache(
        config,
        verifier,
        iap,
        Some(RawStore::Memory(Default::default())),
        cache_store,
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let wait = |mut stopped: tokio::sync::watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };
    tokio::spawn(aster_srv::serve(
        Arc::clone(&app),
        listener,
        wait(stopped.clone()),
    ));
    let api_url = if app.config.api_listen.is_some() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(aster_srv::serve_api(
            Arc::clone(&app),
            listener,
            wait(stopped),
        ));
        Some(api_url)
    } else {
        None
    };
    (app, url, api_url, shutdown)
}

impl Server {
    /// Another server process's worth of app on this server's database: a
    /// second replica, or the same server after a configuration change.
    /// `changes` override the settings this server started with. It does
    /// not own the database; stop the first server last.
    pub async fn second(&self, changes: &[(&str, &str)]) -> Server {
        let mut env: Vec<(&str, &str)> = changes.to_vec();
        env.extend(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let (app, url, api_url, shutdown) = launch(&self.database_url, &env, None).await;
        Server {
            app,
            url,
            api_url,
            admin_url: self.admin_url.clone(),
            database: self.database.clone(),
            database_url: self.database_url.clone(),
            env: self.env.clone(),
            owns_database: false,
            shutdown: Some(shutdown),
        }
    }
}

impl Server {
    /// `POST path` with JSON. `bearer` is an `Authorization` value already
    /// including `Bearer `; `iap` is an IAP assertion.
    pub async fn post_json(
        &self,
        path: &str,
        body: &Value,
        bearer: Option<&str>,
        iap: Option<&str>,
    ) -> (u16, Value) {
        let mut request = aster_srv::http_client()
            .build()
            .unwrap()
            .post(format!("{}{path}", self.url))
            .json(body);
        if let Some(bearer) = bearer {
            request = request.header("Authorization", bearer);
        }
        if let Some(iap) = iap {
            request = request.header("x-goog-iap-jwt-assertion", iap);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(json!(null)))
    }
}

impl Server {
    /// `GET path` returning JSON. `bearer` is an `Authorization` value
    /// including `Bearer `; `iap` is an IAP assertion.
    pub async fn get_json(
        &self,
        path: &str,
        bearer: Option<&str>,
        iap: Option<&str>,
    ) -> (u16, Value) {
        let mut request = aster_srv::http_client()
            .build()
            .unwrap()
            .get(format!("{}{path}", self.url));
        if let Some(bearer) = bearer {
            request = request.header("Authorization", bearer);
        }
        if let Some(iap) = iap {
            request = request.header("x-goog-iap-jwt-assertion", iap);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(json!(null)))
    }
}

impl Server {
    /// `GET path` as a browser behind IAP: the status, the headers and the
    /// page. `assertion` is the `x-goog-iap-jwt-assertion` value, if any.
    pub async fn get_page(
        &self,
        path: &str,
        assertion: Option<&str>,
    ) -> (u16, reqwest::header::HeaderMap, String) {
        let mut request = aster_srv::http_client()
            .build()
            .unwrap()
            .get(format!("{}{path}", self.url));
        if let Some(assertion) = assertion {
            request = request.header("x-goog-iap-jwt-assertion", assertion);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        (status, headers, response.text().await.unwrap())
    }
}

/// An `Authorization` value holding a person token for `email`, as
/// `aster cache login` would have fetched it.
pub fn person_bearer(server: &Server, email: &str) -> String {
    let issuer = server
        .app
        .person_tokens
        .as_ref()
        .expect("person sign-in is configured");
    format!(
        "Bearer {}",
        issuer.issue(&iap_subject(email), email).unwrap().token
    )
}

/// The stable IAP subject the fixtures give `email`.
pub fn iap_subject(email: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(email.as_bytes());
    format!(
        "accounts.google.com:{}",
        digest[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// An IAP assertion for `email`, signed with the fixture key.
pub fn iap_assertion(email: &str) -> String {
    iap_assertion_for(email, IAP_AUDIENCE)
}

/// [`iap_assertion`] addressed to `audience`.
pub fn iap_assertion_for(email: &str, audience: &str) -> String {
    iap_assertion_issued(email, audience, Utc::now().timestamp())
}

/// [`iap_assertion_for`] issued at `iat` (Unix seconds).
pub fn iap_assertion_issued(email: &str, audience: &str, iat: i64) -> String {
    iap_assertion_full(email, Some(&iap_subject(email)), audience, iat)
}

/// An assertion with no `sub` claim at all.
pub fn iap_assertion_without_subject(email: &str) -> String {
    iap_assertion_full(email, None, IAP_AUDIENCE, Utc::now().timestamp())
}

/// An assertion for a chosen subject, as when an account's email changes.
pub fn iap_assertion_with_subject(email: &str, subject: &str) -> String {
    iap_assertion_full(email, Some(subject), IAP_AUDIENCE, Utc::now().timestamp())
}

fn iap_assertion_full(email: &str, subject: Option<&str>, audience: &str, iat: i64) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("iap".into());
    encode(
        &header,
        &json!({
            "iss": "https://cloud.google.com/iap",
            "aud": audience,
            "iat": iat,
            "exp": Utc::now().timestamp() + 300,
            "email": email,
            "sub": subject,
        }),
        &EncodingKey::from_ec_der(&iap_key_pair().0),
    )
    .unwrap()
}

/// An OIDC token for `repository` on `workflow_ref` (`None` omits `ref`).
pub fn token_with_ref(repository: &str, workflow_ref: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    let mut claims = json!({
        "iss": "https://token.actions.githubusercontent.com",
        "aud": "aster-srv",
        "exp": Utc::now().timestamp() + 300,
        "repository": repository,
        "repository_owner_id": "4242",
        "event_name": "push",
    });
    if let Some(workflow_ref) = workflow_ref {
        claims["ref"] = json!(workflow_ref);
    }
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(PRIVATE).unwrap(),
    )
    .unwrap()
}

/// An OIDC token for `repository` with extra claims merged in, such as
/// `ref`, `run_id`, `sha` and `workflow_ref`.
pub fn token_with_claims(repository: &str, extra: Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    let mut claims = json!({
        "iss": "https://token.actions.githubusercontent.com",
        "aud": "aster-srv",
        "exp": Utc::now().timestamp() + 300,
        "repository": repository,
        "repository_owner_id": "4242",
        "event_name": "push",
    });
    for (name, value) in extra.as_object().expect("claims are an object") {
        claims[name] = value.clone();
    }
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(PRIVATE).unwrap(),
    )
    .unwrap()
}

pub fn token(repository: &str) -> String {
    token_with_ref(repository, None)
}

/// One run to put in the database. Its single target is `//a:test`.
pub struct Run<'a> {
    pub repo: &'a str,
    pub run_id: &'a str,
    pub branch: &'a str,
    pub sha: &'a str,
    pub at: DateTime<Utc>,
    pub input_hash: &'a str,
    pub cached: bool,
    pub confidence: Option<&'a str>,
    pub job: Option<&'a str>,
    pub lane: Option<&'a str>,
    /// The CI attempt.
    pub attempt: Option<i32>,
    /// `(test_id, outcome, attempt, failure_hash)`.
    pub results: Vec<(&'a str, &'a str, i32, Option<&'a str>)>,
}

impl<'a> Run<'a> {
    pub fn new(run_id: &'a str, branch: &'a str, at: DateTime<Utc>) -> Self {
        Self {
            repo: REPO,
            run_id,
            branch,
            sha: run_id,
            at,
            input_hash: "h1",
            cached: false,
            confidence: Some("complete"),
            job: None,
            lane: None,
            attempt: None,
            results: Vec::new(),
        }
    }

    pub fn hash(mut self, input_hash: &'a str) -> Self {
        self.input_hash = input_hash;
        self
    }

    pub fn job(mut self, job: &'a str, lane: &'a str, attempt: i32) -> Self {
        self.job = Some(job);
        self.lane = Some(lane);
        self.attempt = Some(attempt);
        self
    }

    pub fn result(mut self, test: &'a str, outcome: &'a str, failure: Option<&'a str>) -> Self {
        self.results.push((test, outcome, 1, failure));
        self
    }

    pub fn attempt(
        mut self,
        test: &'a str,
        outcome: &'a str,
        attempt: i32,
        failure: Option<&'a str>,
    ) -> Self {
        self.results.push((test, outcome, attempt, failure));
        self
    }
}

/// Insert a run, as ingest would, received at `run.at`.
pub async fn seed(server: &Server, run: &Run<'_>) {
    let client = server.client().await;
    let day = run.at.date_naive();
    // A CI job's runner, named after its lane.
    let runner = run.lane.map(|lane| format!("runner-{lane}"));
    aster_srv::test_results::ensure_partitions(&client, day)
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO test_results.runs \
             (run_id, repo, sha, branch, job, lane, attempt, runner, aster_version, \
              started_at, received_at) \
             VALUES ($1, $2, $3, $4, $6, $7, $8, $9, '0', $5, $5)",
            &[
                &run.run_id,
                &run.repo,
                &run.sha,
                &run.branch,
                &run.at,
                &run.job,
                &run.lane,
                &run.attempt,
                &runner,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO test_results.target_runs \
             (received_on, run_id, repo, address, input_hash, status, cached, duration_ms, confidence) \
             VALUES ($1, $2, $3, '//a:test', $4, 'passed', $5, 1, $6)",
            &[
                &day,
                &run.run_id,
                &run.repo,
                &run.input_hash,
                &run.cached,
                &run.confidence,
            ],
        )
        .await
        .unwrap();
    for (test, outcome, attempt, failure) in &run.results {
        let message = failure.map(|_| format!("boom in {test}"));
        client
            .execute(
                "INSERT INTO test_results.test_results \
                 (received_on, run_id, repo, target, test_id, outcome, attempt, duration_ms, \
                  failure_hash, message) \
                 VALUES ($1, $2, $3, '//a:test', $4, $5, $6, 7, $7, $8)",
                &[
                    &day,
                    &run.run_id,
                    &run.repo,
                    test,
                    outcome,
                    attempt,
                    failure,
                    &message,
                ],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO test_results.tests (repo, test_id, target, file, name, first_seen, last_seen) \
                 VALUES ($1, $2, '//a:test', $3, $2, $4, $4) \
                 ON CONFLICT (repo, test_id) DO UPDATE SET \
                     last_seen = GREATEST(test_results.tests.last_seen, EXCLUDED.last_seen)",
                &[&run.repo, test, &format!("test/{test}_test.exs"), &run.at],
            )
            .await
            .unwrap();
    }
}
