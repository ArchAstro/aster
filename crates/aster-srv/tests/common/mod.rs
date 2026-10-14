//! Shared by the classifier and query tests: a server on a fresh database,
//! tokens, and a way to put runs into the database at chosen times.
//!
//! Set ASTER_SRV_TEST_DATABASE_URL to a Postgres URL whose user may create
//! databases; without it `start` returns `None` and the tests skip.
#![allow(dead_code)]

use std::sync::Arc;

use aster_srv::auth::Verifier;
use aster_srv::raw_store::RawStore;
use aster_srv::{App, Config};
use chrono::{DateTime, Utc};
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use serde_json::{json, Value};

const PRIVATE: &[u8] = include_bytes!("../fixtures/test-oidc-key.pem");
const PUBLIC: &[u8] = include_bytes!("../fixtures/test-oidc-key.pub.pem");
pub const REPO: &str = "ArchAstro/firstlanding";

pub struct Server {
    pub app: Arc<App>,
    pub url: String,
    admin_url: String,
    database: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Server {
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.app.pool.close();
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

    let config = Config::from_env(|name| match name {
        "ASTER_SRV_DATABASE_URL" => Some(database_url.clone()),
        "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
        other => env
            .iter()
            .find(|(key, _)| *key == other)
            .map(|(_, value)| value.to_string()),
    })
    .unwrap();
    let verifier =
        Verifier::new(&config).with_key("test", DecodingKey::from_rsa_pem(PUBLIC).unwrap());
    let app = App::start_with(config, verifier, Some(RawStore::Memory(Default::default())))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(aster_srv::serve(Arc::clone(&app), listener, async {
        let _ = stopped.await;
    }));
    Some(Server {
        app,
        url,
        admin_url,
        database,
        shutdown: Some(shutdown),
    })
}

pub fn token(repository: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    encode(
        &header,
        &json!({
            "iss": "https://token.actions.githubusercontent.com",
            "aud": "aster-srv",
            "exp": Utc::now().timestamp() + 300,
            "repository": repository,
        }),
        &EncodingKey::from_rsa_pem(PRIVATE).unwrap(),
    )
    .unwrap()
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
            results: Vec::new(),
        }
    }

    pub fn hash(mut self, input_hash: &'a str) -> Self {
        self.input_hash = input_hash;
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
    aster_srv::test_results::ensure_partitions(&client, day)
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO test_results.runs \
             (run_id, repo, sha, branch, aster_version, started_at, received_at) \
             VALUES ($1, $2, $3, $4, '0', $5, $5)",
            &[&run.run_id, &run.repo, &run.sha, &run.branch, &run.at],
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
