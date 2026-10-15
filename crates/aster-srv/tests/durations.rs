//! `GET /v1/test-results/durations` against a real Postgres.

mod common;

use aster_srv::test_results::durations::window;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use common::{start, Server, REPO};
use serde_json::json;

/// A run of `target` received at `at` whose tests are `(file, test, ms,
/// attempt)`.
async fn seed(
    server: &Server,
    run_id: &str,
    target: &str,
    at: DateTime<Utc>,
    cached: bool,
    confidence: &str,
    tests: &[(&str, &str, i64, i32)],
) {
    let client = server.client().await;
    let day = at.date_naive();
    aster_srv::test_results::ensure_partitions(&client, day)
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO test_results.runs (run_id, repo, sha, aster_version, started_at, received_at) \
             VALUES ($1, $2, 'sha', '0', $3, $3)",
            &[&run_id, &REPO, &at],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO test_results.target_runs \
             (received_on, run_id, repo, address, status, cached, duration_ms, confidence) \
             VALUES ($1, $2, $3, $4, 'passed', $5, 1, $6)",
            &[&day, &run_id, &REPO, &target, &cached, &confidence],
        )
        .await
        .unwrap();
    for (file, test, ms, attempt) in tests {
        let test_id = format!("{file}#{test}");
        client
            .execute(
                "INSERT INTO test_results.test_results \
                 (received_on, run_id, repo, target, test_id, outcome, attempt, duration_ms) \
                 VALUES ($1, $2, $3, $4, $5, 'pass', $6, $7)",
                &[&day, &run_id, &REPO, &target, &test_id, attempt, ms],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO test_results.tests (repo, test_id, target, file, name, first_seen, last_seen) \
                 VALUES ($1, $2, $3, $4, $5, $6, $6) ON CONFLICT DO NOTHING",
                &[&REPO, &test_id, &target, file, test, &at],
            )
            .await
            .unwrap();
    }
}

const QUERY: &str =
    "/v1/test-results/durations?target=%2F%2Fshard-1%3Atest&target=%2F%2Fshard-2%3Atest";

#[tokio::test]
async fn medians_each_file_over_the_shards_runs_in_the_window() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let today = Utc::now().date_naive();
    let (from, to) = window(today);
    let inside = to - Duration::hours(1);
    // a.exs ran on shard 1 (two tests: 10+20, then 30+40) and moved to shard
    // 2 (50+60): per run 30, 70, 110, median 70. The retry attempt is left out.
    seed(
        &server,
        "r1",
        "//shard-1:test",
        inside,
        false,
        "complete",
        &[
            ("test/a_test.exs", "x", 10, 1),
            ("test/a_test.exs", "y", 20, 1),
            ("test/a_test.exs", "y", 999, 2),
        ],
    )
    .await;
    seed(
        &server,
        "r2",
        "//shard-1:test",
        inside - Duration::hours(1),
        false,
        "complete",
        &[
            ("test/a_test.exs", "x", 30, 1),
            ("test/a_test.exs", "y", 40, 1),
            ("test/b_test.exs", "z", 5, 1),
        ],
    )
    .await;
    seed(
        &server,
        "r3",
        "//shard-2:test",
        inside,
        false,
        "complete",
        &[
            ("test/a_test.exs", "x", 50, 1),
            ("test/a_test.exs", "y", 60, 1),
        ],
    )
    .await;
    // Left out: cached, partial, another target, before and after the window.
    seed(
        &server,
        "c",
        "//shard-1:test",
        inside,
        true,
        "complete",
        &[("test/c_test.exs", "c", 1, 1)],
    )
    .await;
    seed(
        &server,
        "p",
        "//shard-1:test",
        inside,
        false,
        "partial",
        &[("test/c_test.exs", "c", 1, 1)],
    )
    .await;
    seed(
        &server,
        "o",
        "//other:test",
        inside,
        false,
        "complete",
        &[("test/c_test.exs", "c", 1, 1)],
    )
    .await;
    seed(
        &server,
        "old",
        "//shard-1:test",
        from - Duration::minutes(1),
        false,
        "complete",
        &[("test/c_test.exs", "c", 1, 1)],
    )
    .await;
    seed(
        &server,
        "new",
        "//shard-1:test",
        to,
        false,
        "complete",
        &[("test/c_test.exs", "c", 1, 1)],
    )
    .await;

    let (status, body) = server
        .get(&format!("{QUERY}&day={today}"), Some(REPO))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["targets"], json!(["//shard-1:test", "//shard-2:test"]));
    assert_eq!(
        body["files"],
        json!([
            {"file": "test/a_test.exs", "p50_ms": 70, "runs": 3},
            {"file": "test/b_test.exs", "p50_ms": 5, "runs": 1},
        ])
    );

    // The same question asked later gets the same answer.
    seed(
        &server,
        "later",
        "//shard-2:test",
        Utc::now(),
        false,
        "complete",
        &[("test/a_test.exs", "x", 1, 1)],
    )
    .await;
    let (_, again) = server
        .get(&format!("{QUERY}&day={today}"), Some(REPO))
        .await;
    assert_eq!(again, body);

    // A day that has not begun is refused; another repository is forbidden.
    let tomorrow = today.succ_opt().unwrap();
    let (status, _) = server
        .get(&format!("{QUERY}&day={tomorrow}"), Some(REPO))
        .await;
    assert_eq!(status, 409);
    let (status, _) = server
        .get(
            &format!("{QUERY}&day={today}&repo=ArchAstro%2Fother"),
            Some(REPO),
        )
        .await;
    assert_eq!(status, 403);
    let (status, _) = server.get(&format!("{QUERY}&day={today}"), None).await;
    assert_eq!(status, 401);
    let (status, empty) = server
        .get(
            &format!(
                "{QUERY}&day={}",
                NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()
            ),
            Some(REPO),
        )
        .await;
    assert_eq!((status, &empty["files"]), (200, &json!([])));
    server.stop().await;
}

#[tokio::test]
async fn keeps_only_each_targets_latest_runs() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let today = Utc::now().date_naive();
    let (_, to) = window(today);
    // 25 runs: the 5 oldest take 1000 ms, the 20 newest 10 ms.
    for i in 0..25 {
        let ms = if i < 5 { 1000 } else { 10 };
        let at = to - Duration::minutes(60 - i);
        seed(
            &server,
            &format!("r{i:02}"),
            "//shard-1:test",
            at,
            false,
            "complete",
            &[("test/a_test.exs", "x", ms, 1)],
        )
        .await;
    }
    let (status, body) = server
        .get(&format!("{QUERY}&day={today}"), Some(REPO))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["files"],
        json!([{"file": "test/a_test.exs", "p50_ms": 10, "runs": 20}])
    );
    server.stop().await;
}
