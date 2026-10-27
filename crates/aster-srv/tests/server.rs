//! aster-srv against a real Postgres, over HTTP.
//!
//! Set ASTER_SRV_TEST_DATABASE_URL to a Postgres URL whose user may create
//! databases (`postgres://postgres:postgres@localhost:5432/postgres`);
//! each test gets its own database. Without it the tests skip.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster_results_model::{
    to_ndjson, Confidence, Record, RunRecord, TargetRecord, TargetStatus, TestOutcome, TestRecord,
};
use aster_srv::auth::Verifier;
use aster_srv::raw_store::RawStore;
use aster_srv::{App, Config};
use chrono::{Days, Utc};
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use serde_json::json;

const PRIVATE: &[u8] = include_bytes!("fixtures/test-oidc-key.pem");
const PUBLIC: &[u8] = include_bytes!("fixtures/test-oidc-key.pub.pem");
const REPO: &str = "ArchAstro/firstlanding";

struct Server {
    app: Arc<App>,
    url: String,
    admin_url: String,
    database: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Server {
    async fn stop(mut self) {
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
}

/// A server on a fresh database, or `None` when no test database is set.
async fn start(retention_days: u32) -> Option<Server> {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return None;
    };
    // Tests start in parallel, and the clock alone can repeat.
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
        "ASTER_SRV_RETENTION_DAYS" => Some(retention_days.to_string()),
        "ASTER_SRV_MAX_BODY_BYTES" => Some((64 * 1024 * 1024).to_string()),
        _ => None,
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

fn token(repository: &str) -> String {
    job_token(repository, "16161", "9f1c")
}

/// The OIDC token of a job in GitHub run `run` (attempt 1) at `sha`.
fn job_token(repository: &str, run: &str, sha: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    encode(
        &header,
        &json!({
            "iss": "https://token.actions.githubusercontent.com",
            "aud": "aster-srv",
            "exp": Utc::now().timestamp() + 300,
            "repository": repository,
            // The job that uploads: pull request 16161's merge commit 9f1c,
            // GitHub run 16161 attempt 1. Its run ids start
            // `gh:<repo>:16161:1:` (see `stream`).
            "run_id": run,
            "run_attempt": "1",
            "sha": sha,
            "ref": "refs/pull/16161/merge",
            "event_name": "pull_request",
        }),
        &EncodingKey::from_rsa_pem(PRIVATE).unwrap(),
    )
    .unwrap()
}

/// The Aster run id `name` in the uploading job's GitHub run.
fn gh(name: &str) -> String {
    format!("gh:{REPO}:16161:1:{name}")
}

fn stream(run_id: &str, repo: &str, tests: usize) -> Vec<Record> {
    let mut records = vec![
        Record::Run(RunRecord {
            run_id: run_id.into(),
            repo: repo.into(),
            sha: "9f1c".into(),
            branch: Some("main".into()),
            pr: Some(16161),
            workflow: Some("CI".into()),
            job: Some("test-ci".into()),
            attempt: Some(1),
            lane: Some("services-platform".into()),
            runner: Some("blacksmith-8vcpu-ubuntu-2404".into()),
            aster_version: "0.18.0".into(),
            started_at: "2026-10-13T17:02:11Z".into(),
        }),
        Record::Target(TargetRecord {
            address: "//services/platform:test-ci".into(),
            input_hash: Some("sha256:4be2".into()),
            status: TargetStatus::Failed,
            cached: false,
            duration_ms: 412_233,
            capture: Some("exunit".into()),
            confidence: Some(Confidence::Complete),
            exit_code: Some(2),
        }),
    ];
    for i in 0..tests {
        let failed = i == 0;
        records.push(Record::Test(TestRecord {
            target: "//services/platform:test-ci".into(),
            test_id: format!("{i:064x}"),
            file: Some(format!("test/t{i}_test.exs")),
            suite: Some("Platform.Test".into()),
            name: format!("test {i}"),
            outcome: if failed {
                TestOutcome::Fail
            } else {
                TestOutcome::Pass
            },
            attempt: 1,
            duration_ms: Some(5),
            failure_hash: failed.then(|| "f3a9".into()),
            message: failed.then(|| "** (DBConnection.OwnershipError) boom".into()),
            quarantined: false,
        }));
    }
    records
}

fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

async fn upload(server: &Server, bearer: Option<&str>, body: Vec<u8>) -> (u16, serde_json::Value) {
    let mut request = aster_srv::http_client()
        .build()
        .unwrap()
        .post(format!("{}/v1/test-results/runs", server.url))
        .header("Content-Type", "application/x-ndjson")
        .header("Content-Encoding", "gzip")
        .body(body);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(json!(null)))
}

async fn count(server: &Server, sql: &str) -> i64 {
    let client = server.app.pool.get().await.unwrap();
    client.query_one(sql, &[]).await.unwrap().get(0)
}

#[tokio::test]
async fn stores_a_run_once_and_keeps_the_raw_upload() {
    let Some(server) = start(30).await else {
        return;
    };
    let body = gzip(&to_ndjson(&stream(&gh("run-1"), REPO, 3)));

    let (status, reply) = upload(&server, Some(&token(REPO)), body.clone()).await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!(
        reply,
        json!({"run_id": gh("run-1"), "duplicate": false, "targets": 1, "tests": 3})
    );

    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.runs").await,
        1
    );
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.target_runs").await,
        1
    );
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.test_results").await,
        3
    );
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.tests").await,
        3
    );
    let client = server.app.pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT outcome, failure_hash, message FROM test_results.test_results WHERE test_id = $1",
            &[&format!("{:064x}", 0)],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "fail");
    assert_eq!(row.get::<_, Option<String>>(1).as_deref(), Some("f3a9"));
    let row = client
        .query_one(
            "SELECT pr, lane, input_hash, confidence, exit_code FROM test_results.runs \
             JOIN test_results.target_runs USING (run_id)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Option<i64>>(0), Some(16161));
    assert_eq!(
        row.get::<_, Option<String>>(1).as_deref(),
        Some("services-platform")
    );
    assert_eq!(
        row.get::<_, Option<String>>(2).as_deref(),
        Some("sha256:4be2")
    );
    assert_eq!(row.get::<_, Option<String>>(3).as_deref(), Some("complete"));
    assert_eq!(row.get::<_, Option<i32>>(4), Some(2));
    drop(client);

    // The same run again changes nothing.
    let (status, reply) = upload(&server, Some(&token(REPO)), body.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(reply["duplicate"], true);
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.test_results").await,
        3
    );

    let RawStore::Memory(objects) = &server.app.raw_store else {
        unreachable!()
    };
    let objects = objects.lock().await;
    assert_eq!(objects.len(), 1, "only the first upload is kept");
    assert!(objects[0]
        .0
        .starts_with("test-results/ArchAstro/firstlanding/"));
    assert_eq!(objects[0].1.as_ref(), body.as_slice());
    drop(objects);
    server.stop().await;
}

#[tokio::test]
async fn a_job_files_results_only_under_its_own_run_commit_and_repository() {
    let Some(server) = start(30).await else {
        return;
    };
    // Setup: GitHub run 16162 has not uploaded yet. A job of run 16161
    // tries to take its run id first, which would make the real upload a
    // "duplicate" and drop its results.
    let next_run = format!("gh:{REPO}:16162:1:test-ci:-:ab");
    let body = gzip(&to_ndjson(&stream(&next_run, REPO, 2)));
    let (status, reply) = upload(&server, Some(&token(REPO)), body.clone()).await;
    assert_eq!(status, 403, "{reply}");

    // The real job of run 16162 uploads its results and they are stored.
    let real = job_token(REPO, "16162", "9f1c");
    let (status, reply) = upload(&server, Some(&real), body).await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!(reply["duplicate"], false);

    // A job cannot file results under a commit it did not run.
    let other_commit = gzip(&to_ndjson(&stream(&gh("other-commit"), REPO, 1)));
    let (status, _) = upload(
        &server,
        Some(&job_token(REPO, "16161", "dead")),
        other_commit,
    )
    .await;
    assert_eq!(status, 403);

    // Another allowed repository cannot use this repository's run ids,
    // with its own name in the record or this one's.
    let theirs = format!("gh:{REPO}:16161:1:theirs");
    for repo in ["ArchAstro/aster", REPO] {
        let body = gzip(&to_ndjson(&stream(&theirs, repo, 1)));
        let (status, _) = upload(&server, Some(&token("ArchAstro/aster")), body).await;
        assert_eq!(status, 403, "{repo}");
    }
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.runs").await,
        1,
        "only the real job's run is stored"
    );
    let source: (String, String, String) = {
        let client = server.app.pool.get().await.unwrap();
        let row = client
            .query_one(
                "SELECT source, git_ref, event_name FROM test_results.runs",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    };
    assert_eq!(
        source,
        (
            "oidc".to_string(),
            "refs/pull/16161/merge".to_string(),
            "pull_request".to_string()
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn refuses_missing_bad_and_mismatched_credentials() {
    let Some(server) = start(30).await else {
        return;
    };
    let body = gzip(&to_ndjson(&stream(&gh("run-2"), REPO, 1)));
    assert_eq!(upload(&server, None, body.clone()).await.0, 401);
    assert_eq!(
        upload(&server, Some("not-a-token"), body.clone()).await.0,
        401
    );
    assert_eq!(
        upload(&server, Some(&token("evil/repo")), body.clone())
            .await
            .0,
        403
    );
    // An allowed repository may not upload another repository's results.
    let (status, reply) = upload(&server, Some(&token("ArchAstro/aster")), body).await;
    assert_eq!(status, 403, "{reply}");
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.runs").await,
        0
    );
    server.stop().await;
}

#[tokio::test]
async fn rejects_malformed_uploads() {
    let Some(server) = start(30).await else {
        return;
    };
    let bearer = token(REPO);
    let (status, reply) = upload(&server, Some(&bearer), gzip("{\"type\":\"nope\"}\n")).await;
    assert_eq!(status, 400);
    assert!(
        reply["error"].as_str().unwrap().contains("line 1"),
        "{reply}"
    );
    let targets_first = to_ndjson(&stream(&gh("run-3"), REPO, 1)[1..]);
    assert_eq!(
        upload(&server, Some(&bearer), gzip(&targets_first)).await.0,
        400
    );
    assert_eq!(
        upload(&server, Some(&bearer), b"not gzip".to_vec()).await.0,
        400
    );
    server.stop().await;
}

#[tokio::test]
async fn health_and_readiness() {
    let Some(server) = start(30).await else {
        return;
    };
    for path in ["/healthz", "/readyz"] {
        let response = reqwest::get(format!("{}{path}", server.url)).await.unwrap();
        assert_eq!(response.status().as_u16(), 200, "{path}");
    }
    let response = reqwest::get(format!("{}/nope", server.url)).await.unwrap();
    assert_eq!(response.status().as_u16(), 404);
    server.stop().await;
}

#[tokio::test]
async fn ingests_fifty_thousand_tests_quickly() {
    let Some(server) = start(30).await else {
        return;
    };
    let body = gzip(&to_ndjson(&stream(&gh("run-big"), REPO, 50_000)));
    let started = Instant::now();
    let (status, reply) = upload(&server, Some(&token(REPO)), body).await;
    let took = started.elapsed();
    assert_eq!(status, 201, "{reply}");
    assert_eq!(reply["tests"], 50_000);
    // The budget holds on a CI runner; a loaded laptop running the other
    // tests at the same time can miss it without anything being wrong.
    if std::env::var_os("CI").is_some() {
        assert!(took < Duration::from_secs(5), "took {took:?}");
    } else if took >= Duration::from_secs(5) {
        eprintln!("warning: 50,000 tests took {took:?}, over the 5s CI budget");
    }
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.tests").await,
        50_000
    );
    server.stop().await;
}

#[tokio::test]
async fn retention_drops_days_past_the_period_and_keeps_the_rest() {
    let Some(server) = start(30).await else {
        return;
    };
    let today = Utc::now().date_naive();
    let client = server.app.pool.get().await.unwrap();
    for age in [31u64, 29] {
        let day = today - Days::new(age);
        aster_srv::test_results::ensure_partitions(&client, day)
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO test_results.test_results \
                 (received_on, run_id, repo, target, test_id, outcome, attempt) \
                 VALUES ($1, 'old', $2, '//a:test', 'x', 'pass', 1)",
                &[&day, &REPO],
            )
            .await
            .unwrap();
        let at = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
        client
            .execute(
                "INSERT INTO test_results.runs (run_id, repo, sha, aster_version, started_at, received_at) \
                 VALUES ($1, $2, 'abc', '0', $3, $3)",
                &[&format!("run-{age}"), &REPO, &at],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO test_results.tests (repo, test_id, target, name, first_seen, last_seen) \
                 VALUES ($1, $2, '//a:test', 't', $3, $3)",
                &[&REPO, &format!("test-{age}"), &at],
            )
            .await
            .unwrap();
    }
    drop(client);

    let report = aster_srv::test_results::retention::run_once(&server.app, today)
        .await
        .unwrap()
        .expect("the lock is free");
    let old = today - Days::new(31);
    assert_eq!(
        report.dropped,
        vec![
            aster_srv::test_results::partition_name("target_runs", old),
            aster_srv::test_results::partition_name("test_flake_events", old),
            aster_srv::test_results::partition_name("test_results", old),
        ]
    );
    assert_eq!(report.runs_deleted, 1);
    assert_eq!(report.tests_deleted, 1);
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.test_results").await,
        1
    );
    let client = server.app.pool.get().await.unwrap();
    let kept: String = client
        .query_one("SELECT run_id FROM test_results.runs", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, "run-29");
    drop(client);

    // Running it again is harmless, and the coming days have partitions.
    let again = aster_srv::test_results::retention::run_once(&server.app, today)
        .await
        .unwrap()
        .unwrap();
    assert!(again.dropped.is_empty());
    let tomorrow = aster_srv::test_results::partition_name("test_results", today + Days::new(2));
    assert_eq!(
        count(
            &server,
            &format!("SELECT count(*) FROM pg_class WHERE relname = '{tomorrow}'")
        )
        .await,
        1
    );
    server.stop().await;
}

#[tokio::test]
async fn migrations_are_applied_once() {
    let Some(server) = start(30).await else {
        return;
    };
    aster_srv::db::migrate(&server.app.pool, &aster_srv::migrations())
        .await
        .unwrap();
    assert_eq!(
        count(&server, "SELECT count(*) FROM aster_srv_migrations").await,
        aster_srv::migrations().len() as i64
    );
    server.stop().await;
}

/// Set ASTER_SRV_TEST_TLS_DATABASE_URL to a TLS-enabled Postgres URL with
/// `sslmode=require`; the connection must then be encrypted.
#[tokio::test]
async fn require_mode_encrypts_the_connection() {
    let Ok(url) = std::env::var("ASTER_SRV_TEST_TLS_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_TLS_DATABASE_URL is not set");
        return;
    };
    let pool = aster_srv::db::pool(&url).unwrap();
    let client = pool.get().await.unwrap();
    let encrypted: bool = client
        .query_one(
            "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(encrypted);
}

#[tokio::test]
async fn disable_mode_connects_in_plain_text() {
    let Ok(url) = std::env::var("ASTER_SRV_TEST_TLS_DATABASE_URL") else {
        return;
    };
    let plain = url.replace("sslmode=require", "sslmode=disable");
    let pool = aster_srv::db::pool(&plain).unwrap();
    let client = pool.get().await.unwrap();
    let encrypted: bool = client
        .query_one(
            "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!encrypted);
}

#[tokio::test]
async fn concurrent_uploads_of_the_same_tests_all_store() {
    let Some(server) = start(30).await else {
        return;
    };
    // Setup: the tests exist, so every upload below updates the same rows.
    let tests = 2_000;
    let (status, reply) = upload(
        &server,
        Some(&token(REPO)),
        gzip(&to_ndjson(&stream(&gh("seed"), REPO, tests))),
    )
    .await;
    assert_eq!(status, 201, "{reply}");

    // Action: shards of one CI run upload at once, as many as one server
    // reads at a time (`ASTER_SRV_MAX_LARGE_BODIES`, 8). Each upsert locks those
    // rows; uploads that locked them in different orders deadlocked, and the
    // loser got a 503.
    let uploads = (0..8).map(|shard| {
        let body = gzip(&to_ndjson(&stream(
            &gh(&format!("shard-{shard}")),
            REPO,
            tests,
        )));
        let server = &server;
        async move { upload(server, Some(&token(REPO)), body).await }
    });
    let replies = futures_util::future::join_all(uploads).await;

    // Outcome: every shard stored, and the tests stay one row each.
    for (status, reply) in &replies {
        assert_eq!(*status, 201, "{reply}");
    }
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.runs").await,
        9
    );
    assert_eq!(
        count(&server, "SELECT count(*) FROM test_results.tests").await,
        tests as i64
    );
    server.stop().await;
}

#[tokio::test]
async fn an_upload_rewrites_a_test_only_when_it_changed_or_an_hour_passed() {
    let Some(server) = start(30).await else {
        return;
    };
    upload_ok(&server, stream(&gh("first"), REPO, 3)).await;
    let first = test_row(&server).await;

    // The same tests again within the hour: the rows are left alone, so
    // uploads do not leave a dead row per test each time.
    upload_ok(&server, stream(&gh("again"), REPO, 3)).await;
    assert_eq!(test_row(&server).await, first);

    // A renamed test is rewritten at once.
    let mut renamed = stream(&gh("renamed"), REPO, 3);
    for record in &mut renamed {
        if let Record::Test(test) = record {
            test.name = format!("{} renamed", test.name);
        }
    }
    upload_ok(&server, renamed).await;
    let after_rename = test_row(&server).await;
    assert_ne!(after_rename.0, first.0);
    assert_eq!(after_rename.2, "test 1 renamed");

    // An hour on, `last_seen` moves again.
    let client = server.app.pool.get().await.unwrap();
    client
        .execute(
            "UPDATE test_results.tests SET last_seen = last_seen - interval '61 minutes'",
            &[],
        )
        .await
        .unwrap();
    drop(client);
    let stale = test_row(&server).await;
    upload_ok(&server, stream(&gh("later"), REPO, 3)).await;
    let later = test_row(&server).await;
    assert!(later.1 > stale.1 + chrono::Duration::minutes(60));
    server.stop().await;
}

/// Test 1's row version (`xmin`), `last_seen` and name.
async fn test_row(server: &Server) -> (i64, chrono::DateTime<Utc>, String) {
    let client = server.app.pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT xmin::text::bigint, last_seen, name FROM test_results.tests WHERE test_id = $1",
            &[&format!("{:064x}", 1)],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1), row.get(2))
}

async fn upload_ok(server: &Server, records: Vec<Record>) {
    let (status, reply) = upload(server, Some(&token(REPO)), gzip(&to_ndjson(&records))).await;
    assert_eq!(status, 201, "{reply}");
}
