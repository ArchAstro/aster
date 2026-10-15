//! The read API (`GET /v1/test-results/...`) against a real Postgres.

mod common;

use std::time::{Duration, Instant};

use aster_srv::test_results::{classifier, query};
use chrono::{DateTime, Duration as Span, Utc};
use common::{seed, start, Run, Server, REPO};
use serde_json::{json, Value};

const OTHER: &str = "ArchAstro/other";

fn ago(now: DateTime<Utc>, minutes: i64) -> DateTime<Utc> {
    now - Span::minutes(minutes)
}

fn ids(page: &Value, key: &str) -> Vec<String> {
    page[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["test_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_tests_by_score_with_a_cursor_and_filters() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    let mut run = Run::new("r1", "main", ago(now, 10));
    for test in ["a", "b", "c", "d", "e"] {
        run = run.result(test, "pass", None);
    }
    seed(&server, &run).await;
    let mut foreign = Run::new("o1", "main", ago(now, 10)).result("z", "pass", None);
    foreign.repo = OTHER;
    seed(&server, &foreign).await;
    server
        .client()
        .await
        .batch_execute(
            "UPDATE test_results.tests SET \
                 state = CASE test_id WHEN 'a' THEN 'flaky' WHEN 'b' THEN 'flaky' \
                     WHEN 'c' THEN 'broken' WHEN 'd' THEN 'flaky' WHEN 'z' THEN 'flaky' \
                     ELSE 'healthy' END, \
                 flake_score = CASE test_id WHEN 'a' THEN 9 WHEN 'b' THEN 9 WHEN 'c' THEN 3 \
                     WHEN 'd' THEN 3 WHEN 'z' THEN 100 ELSE 0 END",
        )
        .await
        .unwrap();

    let (status, all) = server.get("/v1/test-results/tests", Some(REPO)).await;
    assert_eq!(status, 200, "{all}");
    assert_eq!(ids(&all, "tests"), ["a", "b", "c", "d", "e"]);
    assert_eq!(all["next_cursor"], Value::Null);
    assert_eq!(all["repo"], REPO);
    let first = &all["tests"][0];
    assert_eq!(first["state"], "flaky");
    assert_eq!(first["flake_score"], 9);
    assert_eq!(first["name"], "a");
    assert_eq!(first["file"], "test/a_test.exs");
    assert_eq!(first["target"], "//a:test");
    for key in [
        "last_flake_at",
        "broken_since_sha",
        "broken_failure_hash",
        "first_seen",
        "last_seen",
        "suite",
    ] {
        assert!(first.get(key).is_some(), "{key}");
    }

    // Pages of two, following the cursor.
    let mut seen = Vec::new();
    let mut path = "/v1/test-results/tests?limit=2".to_string();
    let mut pages = 0;
    loop {
        let (status, page) = server.get(&path, Some(REPO)).await;
        assert_eq!(status, 200, "{page}");
        seen.extend(ids(&page, "tests"));
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(cursor) => path = format!("/v1/test-results/tests?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(
        (pages, seen),
        (3, ["a", "b", "c", "d", "e"].map(String::from).to_vec())
    );

    // Filtered by state, with the repo spelled out (any case).
    let (_, flaky) = server
        .get(
            "/v1/test-results/tests?state=flaky&repo=archastro/FIRSTLANDING&limit=2",
            Some(REPO),
        )
        .await;
    assert_eq!(ids(&flaky, "tests"), ["a", "b"]);
    let cursor = flaky["next_cursor"].as_str().unwrap();
    let (_, rest) = server
        .get(
            &format!("/v1/test-results/tests?state=flaky&limit=2&cursor={cursor}"),
            Some(REPO),
        )
        .await;
    assert_eq!(ids(&rest, "tests"), ["d"]);
    assert_eq!(rest["next_cursor"], Value::Null);
    let (_, broken) = server
        .get("/v1/test-results/tests?state=broken", Some(REPO))
        .await;
    assert_eq!(ids(&broken, "tests"), ["c"]);

    // The other repository sees only its own.
    let (_, theirs) = server.get("/v1/test-results/tests", Some(OTHER)).await;
    assert_eq!(ids(&theirs, "tests"), ["z"]);

    // Errors.
    assert_eq!(server.get("/v1/test-results/tests", None).await.0, 401);
    let (status, body) = server
        .get(&format!("/v1/test-results/tests?repo={OTHER}"), Some(REPO))
        .await;
    assert_eq!(status, 403, "{body}");
    for bad in [
        "state=wobbly",
        "limit=0",
        "limit=501",
        "limit=many",
        "cursor=zz",
        "cursor=61",
        "cursor=312d310a61",
        "nope=1",
        "state=%ff",
    ] {
        let (status, body) = server
            .get(&format!("/v1/test-results/tests?{bad}"), Some(REPO))
            .await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
    let response = aster_srv::http_client()
        .build()
        .unwrap()
        .post(format!("{}/v1/test-results/tests", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 405);
    server.stop().await;
}

#[tokio::test]
async fn shows_a_tests_state_and_history_newest_first() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(
        &server,
        &Run::new("r1", "pr-1", ago(now, 40)).result("t", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r2", "pr-1", ago(now, 30)).result("t", "fail", Some("f1")),
    )
    .await;
    seed(
        &server,
        &Run::new("r3", "main", ago(now, 20))
            .attempt("t", "fail", 1, Some("f2"))
            .attempt("t", "pass", 2, None),
    )
    .await;
    seed(
        &server,
        &Run::new("r4", "main", ago(now, 10)).result("t", "pass", None),
    )
    .await;
    let mut foreign = Run::new("o1", "main", ago(now, 10)).result("t", "fail", Some("x"));
    foreign.repo = OTHER;
    seed(&server, &foreign).await;
    seed(
        &server,
        &Run::new("o2", "main", ago(now, 5)).result("only-other", "pass", None),
    )
    .await;
    server
        .client()
        .await
        .execute(
            "DELETE FROM test_results.tests WHERE test_id = 'only-other' AND repo = $1",
            &[&REPO],
        )
        .await
        .unwrap();
    classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .unwrap();

    let (status, detail) = server.get("/v1/test-results/tests/t", Some(REPO)).await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["test"]["test_id"], "t");
    assert_eq!(detail["test"]["state"], "flaky");
    assert!(detail["test"]["flake_score"].as_i64().unwrap() >= 3);
    assert!(detail["test"]["last_flake_at"].is_string());
    let history = detail["history"].as_array().unwrap();
    let order: Vec<(&str, i64)> = history
        .iter()
        .map(|h| {
            (
                h["run_id"].as_str().unwrap(),
                h["attempt"].as_i64().unwrap(),
            )
        })
        .collect();
    // Only this repository's results, newest first, later attempts first.
    assert_eq!(
        order,
        [("r4", 1), ("r3", 2), ("r3", 1), ("r2", 1), ("r1", 1)]
    );
    let r3_first = &history[2];
    assert_eq!(r3_first["sha"], "r3");
    assert_eq!(r3_first["branch"], "main");
    assert_eq!(r3_first["outcome"], "fail");
    assert_eq!(r3_first["duration_ms"], 7);
    assert_eq!(r3_first["failure_hash"], "f2");
    assert!(r3_first["observed_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(detail["next_cursor"], Value::Null);

    // Paged.
    let mut seen = Vec::new();
    let mut path = "/v1/test-results/tests/t?limit=2".to_string();
    loop {
        let (status, page) = server.get(&path, Some(REPO)).await;
        assert_eq!(status, 200, "{page}");
        for h in page["history"].as_array().unwrap() {
            seen.push((
                h["run_id"].as_str().unwrap().to_string(),
                h["attempt"].as_i64().unwrap(),
            ));
        }
        match page["next_cursor"].as_str() {
            Some(cursor) => path = format!("/v1/test-results/tests/t?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0], ("r4".to_string(), 1));
    assert_eq!(seen[4], ("r1".to_string(), 1));

    // Errors.
    assert_eq!(
        server
            .get("/v1/test-results/tests/nope", Some(REPO))
            .await
            .0,
        404
    );
    assert_eq!(
        server
            .get("/v1/test-results/tests/only-other", Some(REPO))
            .await
            .0,
        404
    );
    assert_eq!(server.get("/v1/test-results/tests/t", None).await.0, 401);
    assert_eq!(
        server
            .get(
                &format!("/v1/test-results/tests/t?repo={OTHER}"),
                Some(REPO)
            )
            .await
            .0,
        403
    );
    for bad in [
        "limit=0",
        "limit=501",
        "cursor=zz",
        "cursor=313a",
        "state=flaky",
    ] {
        let (status, body) = server
            .get(&format!("/v1/test-results/tests/t?{bad}"), Some(REPO))
            .await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
    // The other repository's `t` is its own.
    let (_, theirs) = server.get("/v1/test-results/tests/t", Some(OTHER)).await;
    assert_eq!(theirs["history"].as_array().unwrap().len(), 1);
    server.stop().await;
}

#[tokio::test]
async fn reports_a_runs_failures_with_their_state() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    for (i, minutes) in [60, 50, 40].into_iter().enumerate() {
        let id = format!("m{i}");
        seed(
            &server,
            &Run::new(&id, "main", ago(now, minutes))
                .hash(&id)
                .result("dead", "fail", Some("d")),
        )
        .await;
    }
    let run_id = "gh:ArchAstro/firstlanding:7:1:job";
    seed(
        &server,
        &Run::new(run_id, "main", ago(now, 10))
            .hash("last")
            .result("dead", "fail", Some("d"))
            .result("fresh", "error", Some("n"))
            .result("fine", "pass", None)
            .result("skipped", "skip", None)
            .attempt("retried", "fail", 1, Some("r"))
            .attempt("retried", "pass", 2, None),
    )
    .await;
    classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .unwrap();

    let path = format!(
        "/v1/test-results/runs/{}/failures",
        run_id.replace('/', "%2F")
    );
    let (status, report) = server.get(&path, Some(REPO)).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["run"]["run_id"], run_id);
    assert_eq!(report["run"]["repo"], REPO);
    assert_eq!(report["run"]["branch"], "main");
    assert_eq!(ids(&report, "failures"), ["dead", "fresh", "retried"]);
    let failures = report["failures"].as_array().unwrap();
    let dead = &failures[0];
    assert_eq!(dead["state"], "broken");
    assert_eq!(dead["broken_since_sha"], "m0");
    assert_eq!(dead["failure_hash"], "d");
    assert_eq!(dead["message"], "boom in dead");
    assert_eq!(dead["file"], "test/dead_test.exs");
    assert_eq!(dead["final_outcome"], "fail");
    assert_eq!(dead["passed_on_retry"], false);
    let fresh = &failures[1];
    assert_eq!(fresh["state"], "new");
    assert_eq!(fresh["final_outcome"], "error");
    assert_eq!(fresh["flake_score"], 0);
    let retried = &failures[2];
    assert_eq!(retried["state"], "flaky");
    assert_eq!(retried["passed_on_retry"], true);
    assert_eq!(retried["failure_hash"], "r");
    assert_eq!(
        retried["attempts"],
        json!([
            {"attempt": 1, "outcome": "fail", "duration_ms": 7},
            {"attempt": 2, "outcome": "pass", "duration_ms": 7},
        ])
    );

    // Paged.
    let (_, first) = server.get(&format!("{path}?limit=2"), Some(REPO)).await;
    assert_eq!(ids(&first, "failures"), ["dead", "fresh"]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, last) = server
        .get(&format!("{path}?limit=2&cursor={cursor}"), Some(REPO))
        .await;
    assert_eq!(ids(&last, "failures"), ["retried"]);
    assert_eq!(last["next_cursor"], Value::Null);

    // A run with no failures has an empty report.
    let (status, clean) = server
        .get("/v1/test-results/runs/m0/failures", Some(REPO))
        .await;
    assert_eq!(status, 200);
    assert_eq!(ids(&clean, "failures"), ["dead"]);
    seed(
        &server,
        &Run::new("green", "main", ago(now, 1)).result("fine", "pass", None),
    )
    .await;
    let (_, green) = server
        .get("/v1/test-results/runs/green/failures", Some(REPO))
        .await;
    assert_eq!(green["failures"], json!([]));

    // Errors.
    assert_eq!(server.get(&path, None).await.0, 401);
    assert_eq!(
        server
            .get("/v1/test-results/runs/nope/failures", Some(REPO))
            .await
            .0,
        404
    );
    let mut foreign = Run::new("o1", "main", ago(now, 5)).result("t", "fail", Some("x"));
    foreign.repo = OTHER;
    seed(&server, &foreign).await;
    assert_eq!(
        server
            .get("/v1/test-results/runs/o1/failures", Some(REPO))
            .await
            .0,
        403
    );
    assert_eq!(
        server
            .get("/v1/test-results/runs/o1/failures", Some(OTHER))
            .await
            .0,
        200
    );
    assert_eq!(
        server
            .get(&format!("{path}?repo={OTHER}"), Some(REPO))
            .await
            .0,
        403
    );
    for bad in ["limit=0", "limit=1001", "state=flaky"] {
        assert_eq!(
            server.get(&format!("{path}?{bad}"), Some(REPO)).await.0,
            400,
            "{bad}"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn reports_the_failures_of_a_github_run() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    let gh = "gh:ArchAstro/firstlanding:900";
    let platform = format!("{gh}:1:test-ci:platform:test:00");
    let web = format!("{gh}:1:test-ci:web:test:01");
    let rerun = format!("{gh}:2:test-ci:platform:test:02");
    // Run 9000 must not match run 900.
    let other_run = "gh:ArchAstro/firstlanding:9000:1:test-ci:x:test:03";
    seed(
        &server,
        &Run::new(&platform, "main", ago(now, 30))
            .job("test-ci", "platform", 1)
            .result("dead", "fail", Some("d"))
            .attempt("retried", "fail", 1, Some("r"))
            .attempt("retried", "pass", 2, None),
    )
    .await;
    seed(
        &server,
        &Run::new(&web, "main", ago(now, 29))
            .job("test-ci", "web", 1)
            .result("fresh", "error", Some("n"))
            .result("fine", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new(&rerun, "main", ago(now, 5))
            .job("test-ci", "platform", 2)
            .result("fine", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new(other_run, "main", ago(now, 5))
            .job("test-ci", "x", 1)
            .result("nine", "fail", Some("z")),
    )
    .await;
    let mut foreign = Run::new("gh:ArchAstro/other:900:1:j:l:t:04", "main", ago(now, 5)).result(
        "theirs",
        "fail",
        Some("x"),
    );
    foreign.repo = OTHER;
    seed(&server, &foreign).await;
    // Seeded now, then aged past retention.
    let old = Run::new(
        "gh:ArchAstro/firstlanding:800:1:j:l:t:05",
        "main",
        ago(now, 5),
    )
    .result("ancient", "fail", Some("a"));
    seed(&server, &old).await;
    server
        .client()
        .await
        .batch_execute(
            "UPDATE test_results.runs SET received_at = now() - interval '40 days' \
             WHERE run_id LIKE 'gh:ArchAstro/firstlanding:800:%'",
        )
        .await
        .unwrap();
    classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .unwrap();

    let path = "/v1/test-results/failures?github_run=900";
    let (status, report) = server.get(path, Some(REPO)).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["github_run"], "900");
    let runs = report["runs"].as_array().unwrap();
    assert_eq!(
        runs.iter()
            .map(|r| r["run_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [platform.as_str(), web.as_str(), rerun.as_str()]
    );
    assert_eq!(runs[1]["lane"], "web");
    assert_eq!(runs[1]["job"], "test-ci");
    assert_eq!(runs[1]["runner"], "runner-web");
    assert_eq!(runs[1]["attempt"], 1);
    assert_eq!(runs[1]["branch"], "main");
    for key in ["sha", "pr", "received_at"] {
        assert!(runs[1].get(key).is_some(), "{key}");
    }
    assert_eq!(ids(&report, "failures"), ["dead", "retried", "fresh"]);
    let failures = report["failures"].as_array().unwrap();
    assert_eq!(failures[0]["run_id"], platform.as_str());
    assert_eq!(failures[0]["lane"], "platform");
    assert_eq!(failures[0]["job"], "test-ci");
    assert_eq!(failures[0]["message"], "boom in dead");
    assert_eq!(failures[1]["passed_on_retry"], true);
    assert_eq!(failures[2]["lane"], "web");
    assert_eq!(failures[2]["runner"], "runner-web");
    assert_eq!(failures[2]["state"], "new");
    assert_eq!(report["next_cursor"], Value::Null);

    // One attempt of the run.
    let (_, second) = server.get(&format!("{path}&attempt=2"), Some(REPO)).await;
    assert_eq!(second["runs"].as_array().unwrap().len(), 1);
    assert_eq!(second["failures"], json!([]));

    // Paged across runs.
    let mut seen = Vec::new();
    let mut next = format!("{path}&limit=1");
    loop {
        let (status, page) = server.get(&next, Some(REPO)).await;
        assert_eq!(status, 200, "{page}");
        seen.extend(ids(&page, "failures"));
        match page["next_cursor"].as_str() {
            Some(cursor) => next = format!("{path}&limit=1&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen, ["dead", "retried", "fresh"]);

    // Errors: unknown, expired, another repository's, bad parameters.
    assert_eq!(server.get(path, None).await.0, 401);
    for missing in ["github_run=1", "github_run=800", "github_run=900&attempt=3"] {
        assert_eq!(
            server
                .get(&format!("/v1/test-results/failures?{missing}"), Some(REPO))
                .await
                .0,
            404,
            "{missing}"
        );
    }
    let (_, theirs) = server.get(path, Some(OTHER)).await;
    assert_eq!(ids(&theirs, "failures"), ["theirs"]);
    for bad in [
        "",
        "github_run=",
        "github_run=9%25",
        "github_run=x",
        "github_run=900&attempt=0",
        "github_run=900&attempt=a",
        "github_run=900&state=flaky",
        "github_run=900&limit=1001",
        "github_run=900&cursor=zz",
        "github_run=900&cursor=61",
    ] {
        assert_eq!(
            server
                .get(&format!("/v1/test-results/failures?{bad}"), Some(REPO))
                .await
                .0,
            400,
            "{bad}"
        );
    }
    assert_eq!(
        server
            .get(&format!("{path}&repo={OTHER}"), Some(REPO))
            .await
            .0,
        403
    );
    // The parameters belong to this route alone.
    assert_eq!(
        server
            .get("/v1/test-results/tests?github_run=900", Some(REPO))
            .await
            .0,
        400
    );
    server.stop().await;
}

/// 20,000 tests in each of 10 runs over three days: 200,000 results.
async fn seed_large(server: &Server, now: DateTime<Utc>) {
    let client = server.client().await;
    for age in 0..4 {
        let day = (now - Span::days(age)).date_naive();
        aster_srv::test_results::ensure_partitions(&client, day)
            .await
            .unwrap();
    }
    client
        .batch_execute(&format!(
            "INSERT INTO test_results.runs (run_id, repo, sha, branch, aster_version, started_at, received_at) \
             SELECT 'big-' || r, '{REPO}', 'sha' || r, CASE WHEN r % 2 = 0 THEN 'main' ELSE 'pr' END, '0', \
                    now() - (r || ' hours')::interval * 7, now() - (r || ' hours')::interval * 7 \
             FROM generate_series(1, 10) r; \
             INSERT INTO test_results.target_runs (received_on, run_id, repo, address, input_hash, status, cached, duration_ms, confidence) \
             SELECT (received_at AT TIME ZONE 'UTC')::date, run_id, repo, '//big:test', 'same', 'passed', false, 1, 'complete' \
             FROM test_results.runs; \
             INSERT INTO test_results.test_results (received_on, run_id, repo, target, test_id, outcome, attempt, duration_ms, failure_hash, message) \
             SELECT (r.received_at AT TIME ZONE 'UTC')::date, r.run_id, r.repo, '//big:test', 'test-' || lpad(i::text, 6, '0'), \
                    CASE WHEN i % 50 = substr(r.run_id, 5)::int THEN 'fail' ELSE 'pass' END, 1, 5, \
                    CASE WHEN i % 50 = substr(r.run_id, 5)::int THEN 'hash-' || (i % 50) END, \
                    CASE WHEN i % 50 = substr(r.run_id, 5)::int THEN 'it broke' END \
             FROM test_results.runs r CROSS JOIN generate_series(1, 20000) i; \
             INSERT INTO test_results.tests (repo, test_id, target, file, name, first_seen, last_seen) \
             SELECT '{REPO}', 'test-' || lpad(i::text, 6, '0'), '//big:test', 'f.exs', 'test ' || i, now(), now() \
             FROM generate_series(1, 20000) i; \
             ANALYZE test_results.test_results; ANALYZE test_results.tests; \
             ANALYZE test_results.runs; ANALYZE test_results.target_runs;"
        ))
        .await
        .unwrap();
}

/// Time a request and require it to be quick. Budgets are generous: the
/// target is p95 under 300 ms on a real database, and a shared CI runner
/// should still be far under two seconds.
async fn quick(server: &Server, path: &str) -> Value {
    let started = Instant::now();
    let (status, body) = server.get(path, Some(REPO)).await;
    let took = started.elapsed();
    assert_eq!(status, 200, "{path}: {body}");
    assert!(took < Duration::from_secs(2), "{path} took {took:?}");
    eprintln!("{path}: {took:?}");
    body
}

#[tokio::test]
async fn queries_stay_fast_over_two_hundred_thousand_results() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed_large(&server, now).await;

    // The classifier handles the whole set in pieces, in bounded time.
    let started = Instant::now();
    let report = classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .unwrap();
    let took = started.elapsed();
    eprintln!(
        "classified {} results in {took:?}: {report:?}",
        report.results
    );
    assert!(report.results >= 200_000, "{report:?}");
    assert!(report.events > 0 && report.tests_updated > 0);
    assert!(took < Duration::from_secs(120), "classifying took {took:?}");

    let list = quick(&server, "/v1/test-results/tests?limit=100").await;
    assert_eq!(list["tests"].as_array().unwrap().len(), 100);
    // Failing on main in the middle of the sequence: two flips and a likely.
    assert_eq!(list["tests"][0]["flake_score"], 7);
    let cursor = list["next_cursor"].as_str().unwrap();
    quick(
        &server,
        &format!("/v1/test-results/tests?limit=100&cursor={cursor}"),
    )
    .await;
    let flaky = quick(&server, "/v1/test-results/tests?state=flaky&limit=500").await;
    assert_eq!(flaky["tests"].as_array().unwrap().len(), 500);
    quick(&server, "/v1/test-results/tests?state=healthy&limit=500").await;
    // test-000001 fails in run big-1 only (1 % 50 = 1).
    let detail = quick(&server, "/v1/test-results/tests/test-000001").await;
    assert_eq!(detail["history"].as_array().unwrap().len(), 10);
    let failures = quick(&server, "/v1/test-results/runs/big-3/failures?limit=1000").await;
    assert_eq!(failures["failures"].as_array().unwrap().len(), 400);

    // EXPLAIN: the list reads `tests` along an index, with no sort, and a
    // test's history reads each day's partition through an index.
    let client = server.client().await;
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .unwrap();
    for (with_state, with_cursor) in [(false, false), (true, false), (true, true)] {
        let sql = format!("EXPLAIN {}", query::list_sql(with_state, with_cursor));
        let mut args: Vec<Box<dyn tokio_postgres::types::ToSql + Sync>> =
            vec![Box::new(REPO.to_string())];
        if with_state {
            args.push(Box::new("flaky".to_string()));
        }
        if with_cursor {
            args.push(Box::new(3_i32));
            args.push(Box::new("test-000100".to_string()));
        }
        args.push(Box::new(101_i64));
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            args.iter().map(|a| a.as_ref() as _).collect();
        let plan: String = client
            .query(&sql, &refs)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plan.contains("Index Scan using tests_by_"), "{plan}");
        assert!(!plan.contains("Sort"), "{plan}");
    }
    let since = (now - Span::days(30)).date_naive();
    let plan: String = client
        .query(
            &format!("EXPLAIN {}", query::history_sql(false)),
            &[&REPO, &"test-000001", &since, &101_i64],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan.contains("repo_test_id_received_on_idx"), "{plan}");
    assert!(!plan.contains("Seq Scan on test_results_p"), "{plan}");
    drop(client);
    server.stop().await;
}
