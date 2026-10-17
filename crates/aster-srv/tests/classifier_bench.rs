//! How long a classifier pass takes over a long history. Ignored by
//! default; run with
//! `cargo test -p aster-srv --test classifier_bench -- --ignored --nocapture`
//! and `ASTER_SRV_TEST_DATABASE_URL` set. `BENCH_DAYS` and `BENCH_TESTS`
//! size the history (default 10 days of 2,000 tests, a run every 20
//! minutes).

mod common;

use std::time::Instant;

use aster_srv::test_results::{classifier, ensure_partitions};
use chrono::{Duration, DurationRound, Utc};
use common::start;

fn env_or(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test]
#[ignore]
async fn a_pass_over_two_hours_after_a_long_history() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let days = env_or("BENCH_DAYS", 10);
    let tests = env_or("BENCH_TESTS", 2000);
    let now = Utc::now().duration_trunc(Duration::minutes(20)).unwrap();
    let first = now - Duration::days(days);
    let client = server.client().await;
    let mut day = first.date_naive();
    while day <= now.date_naive() {
        ensure_partitions(&client, day).await.unwrap();
        day = day.succ_opt().unwrap();
    }
    let started = Instant::now();
    // A run every 20 minutes; every third on main; three runs in a row share
    // an input hash. Most tests pass, 2% of results fail, and every 40th
    // test fails a third of the time with one of two messages.
    client
        .batch_execute(&format!(
            "CREATE TEMP TABLE bench_runs AS \
             SELECT n, 'bench-' || n AS run_id, \
                    timestamptz '{first}' + n * interval '20 minutes' AS at, \
                    CASE WHEN n % 3 = 0 THEN 'main' ELSE 'pr-' || (n % 17) END AS branch, \
                    'h' || (n / 3) AS input_hash \
             FROM generate_series(1, {runs}) n \
             WHERE timestamptz '{first}' + n * interval '20 minutes' <= timestamptz '{now}'; \
             INSERT INTO test_results.runs (run_id, repo, sha, branch, aster_version, started_at, received_at) \
             SELECT run_id, 'ArchAstro/bench', 'sha-' || n, branch, '0', at, at FROM bench_runs; \
             INSERT INTO test_results.target_runs \
                 (received_on, run_id, repo, address, input_hash, status, cached, duration_ms, confidence) \
             SELECT (at AT TIME ZONE 'UTC')::date, run_id, 'ArchAstro/bench', '//a:test', input_hash, \
                    'passed', false, 1, 'complete' FROM bench_runs; \
             INSERT INTO test_results.test_results \
                 (received_on, run_id, repo, target, test_id, outcome, attempt, duration_ms, failure_hash) \
             SELECT (r.at AT TIME ZONE 'UTC')::date, r.run_id, 'ArchAstro/bench', '//a:test', 't' || t, \
                    CASE WHEN f THEN 'fail' ELSE 'pass' END, 1, 5, \
                    CASE WHEN f THEN 'f' || (1 + (r.n + t) % 2) END \
             FROM bench_runs r CROSS JOIN generate_series(1, {tests}) t \
             CROSS JOIN LATERAL (SELECT (abs(hashint8(r.n * 100003 + t)) % 100) \
                                        < CASE WHEN t % 40 = 0 THEN 33 ELSE 2 END AS f) o; \
             INSERT INTO test_results.tests (repo, test_id, target, file, name, first_seen, last_seen) \
             SELECT 'ArchAstro/bench', 't' || t, '//a:test', 'test/t' || t || '_test.exs', 't' || t, \
                    timestamptz '{first}', timestamptz '{now}' FROM generate_series(1, {tests}) t; \
             ANALYZE;",
            runs = days * 72,
        ))
        .await
        .unwrap();
    let results: i64 = client
        .query_one("SELECT count(*) FROM test_results.test_results", &[])
        .await
        .unwrap()
        .get(0);
    println!(
        "seeded {results} results ({days} days, {tests} tests) in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    // Catch up to two hours ago: a deploy's first pass.
    let started = Instant::now();
    classifier::run_once(&server.app, now - Duration::hours(2))
        .await
        .unwrap()
        .unwrap();
    println!("catch-up pass: {:.1}s", started.elapsed().as_secs_f64());

    // Then the steady state: one pass over the last two hours.
    let started = Instant::now();
    let report = classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .unwrap();
    println!(
        "two-hour pass: {:.2}s ({} results, {} events)",
        started.elapsed().as_secs_f64(),
        report.results,
        report.events
    );
    server.stop().await;
}
