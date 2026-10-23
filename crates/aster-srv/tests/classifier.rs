//! The flake and broken classifier against a real Postgres. Runs are put in
//! the database at chosen times, then `classifier::run_once` is called with
//! a chosen clock.

mod common;

use aster_srv::test_results::classifier;
use chrono::{DateTime, Duration, Utc};
use common::{seed, start, Run, Server};

/// Minutes before `now`.
fn ago(now: DateTime<Utc>, minutes: i64) -> DateTime<Utc> {
    now - Duration::minutes(minutes)
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    state: String,
    score: i32,
    since_sha: Option<String>,
    failure_hash: Option<String>,
}

async fn state(server: &Server, test: &str) -> State {
    let row = server
        .client()
        .await
        .query_one(
            "SELECT state, flake_score, broken_since_sha, broken_failure_hash \
             FROM test_results.tests WHERE test_id = $1",
            &[&test],
        )
        .await
        .unwrap();
    State {
        state: row.get(0),
        score: row.get(1),
        since_sha: row.get(2),
        failure_hash: row.get(3),
    }
}

/// `(test, kind, reason, run)` of every event, in a stable order.
async fn events(server: &Server) -> Vec<(String, String, String, String)> {
    server
        .client()
        .await
        .query(
            "SELECT test_id, kind, reason, run_id FROM test_results.test_flake_events \
             ORDER BY test_id, observed_at, kind, run_id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
}

fn event(test: &str, kind: &str, reason: &str, run: &str) -> (String, String, String, String) {
    (test.into(), kind.into(), reason.into(), run.into())
}

async fn classify(server: &Server, now: DateTime<Utc>) -> classifier::Report {
    classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .expect("the lock is free")
}

#[tokio::test]
async fn the_same_input_hash_passing_and_failing_is_a_definite_flake() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    // On a pull request branch, with the same hash.
    seed(
        &server,
        &Run::new("r1", "pr-1", ago(now, 50)).result("flip", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r2", "pr-1", ago(now, 40)).result("flip", "fail", Some("f1")),
    )
    .await;
    // The same flip under a different hash is a change, not a flake.
    seed(
        &server,
        &Run::new("r3", "pr-2", ago(now, 30))
            .hash("h2")
            .result("moved", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r4", "pr-2", ago(now, 20))
            .hash("h3")
            .result("moved", "fail", Some("f1")),
    )
    .await;
    // A cached target replays results and says nothing.
    let mut cached = Run::new("r5", "pr-3", ago(now, 10)).result("replayed", "fail", Some("f1"));
    cached.cached = true;
    seed(
        &server,
        &Run::new("r6", "pr-3", ago(now, 15)).result("replayed", "pass", None),
    )
    .await;
    seed(&server, &cached).await;

    classify(&server, now).await;

    assert_eq!(
        events(&server).await,
        vec![event("flip", "definite", "input_hash", "r2")]
    );
    assert_eq!(
        state(&server, "flip").await,
        State {
            state: "flaky".into(),
            score: 3,
            since_sha: None,
            failure_hash: None
        }
    );
    assert_eq!(state(&server, "moved").await.state, "healthy");
    assert_eq!(state(&server, "replayed").await.state, "healthy");
    server.stop().await;
}

#[tokio::test]
async fn failing_then_passing_on_a_retry_in_one_run_is_a_definite_flake() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(
        &server,
        &Run::new("r1", "pr-1", ago(now, 10))
            .attempt("retried", "fail", 1, Some("f1"))
            .attempt("retried", "pass", 2, None)
            // Still failing after its retry: broken-looking, not flaky.
            .attempt("stuck", "fail", 1, Some("f2"))
            .attempt("stuck", "error", 2, Some("f2")),
    )
    .await;
    classify(&server, now).await;
    assert_eq!(
        events(&server).await,
        vec![event("retried", "definite", "retry", "r1")]
    );
    assert_eq!(state(&server, "retried").await.score, 3);
    assert_eq!(state(&server, "stuck").await.state, "healthy");
    server.stop().await;
}

#[tokio::test]
async fn a_main_failure_followed_by_a_main_pass_is_a_likely_flake() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    // Different hashes, so rule 2 cannot apply.
    seed(
        &server,
        &Run::new("m1", "main", ago(now, 50))
            .hash("a")
            .result("blip", "fail", Some("f1")),
    )
    .await;
    seed(
        &server,
        &Run::new("m2", "main", ago(now, 40))
            .hash("b")
            .result("blip", "pass", None),
    )
    .await;
    // Off the default branch the same pattern is nothing.
    seed(
        &server,
        &Run::new("p1", "pr-1", ago(now, 30))
            .hash("a")
            .result("off", "fail", Some("f1")),
    )
    .await;
    seed(
        &server,
        &Run::new("p2", "pr-1", ago(now, 20))
            .hash("b")
            .result("off", "pass", None),
    )
    .await;
    // Partial results are not trusted to judge a failure.
    let mut partial =
        Run::new("m3", "main", ago(now, 10))
            .hash("c")
            .result("fuzzy", "fail", Some("f1"));
    partial.confidence = Some("partial");
    seed(&server, &partial).await;
    seed(
        &server,
        &Run::new("m4", "main", ago(now, 5))
            .hash("d")
            .result("fuzzy", "pass", None),
    )
    .await;

    classify(&server, now).await;

    assert_eq!(
        events(&server).await,
        // The event is the failed run's.
        vec![event("blip", "likely", "next_main_run", "m1")]
    );
    let blip = state(&server, "blip").await;
    assert_eq!((blip.state.as_str(), blip.score), ("flaky", 1));
    assert_eq!(state(&server, "off").await.state, "healthy");
    assert_eq!(state(&server, "fuzzy").await.state, "healthy");
    server.stop().await;
}

#[tokio::test]
async fn a_default_branch_setting_chooses_the_branch() {
    let Some(server) = start(&[("ASTER_SRV_DEFAULT_BRANCH", "trunk")]).await else {
        return;
    };
    let now = Utc::now();
    seed(
        &server,
        &Run::new("t1", "trunk", ago(now, 50))
            .hash("a")
            .result("x", "fail", Some("f")),
    )
    .await;
    seed(
        &server,
        &Run::new("t2", "refs/heads/trunk", ago(now, 40))
            .hash("b")
            .result("x", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("m1", "main", ago(now, 30))
            .hash("a")
            .result("y", "fail", Some("f")),
    )
    .await;
    seed(
        &server,
        &Run::new("m2", "main", ago(now, 20))
            .hash("b")
            .result("y", "pass", None),
    )
    .await;
    classify(&server, now).await;
    assert_eq!(
        events(&server).await,
        vec![event("x", "likely", "next_main_run", "t1")]
    );
    server.stop().await;
}

#[tokio::test]
async fn consecutive_same_failures_on_main_break_a_test_until_it_passes() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    for (i, minutes) in [100, 90, 80, 70].into_iter().enumerate() {
        let id = format!("m{i}");
        let hash = format!("hash{i}");
        seed(
            &server,
            &Run::new(&id, "main", ago(now, minutes))
                .hash(&hash)
                .result("broke", "fail", Some("same"))
                // Failing, but not the same way each time.
                .result("varied", "fail", Some(&format!("f{i}")))
                // Only two in a row.
                .result("short", if i >= 2 { "fail" } else { "pass" }, Some("same")),
        )
        .await;
    }
    classify(&server, now).await;
    assert_eq!(
        state(&server, "broke").await,
        State {
            state: "broken".into(),
            score: 0,
            since_sha: Some("m0".into()),
            failure_hash: Some("same".into()),
        }
    );
    assert_eq!(state(&server, "varied").await.state, "healthy");
    assert_eq!(state(&server, "short").await.state, "healthy");
    assert!(events(&server).await.is_empty());

    // The fix lands: a pass clears it, and fixing is not flaking.
    seed(
        &server,
        &Run::new("m4", "main", ago(now, 5))
            .hash("fixed")
            .result("broke", "pass", None),
    )
    .await;
    classify(&server, now).await;
    assert_eq!(
        state(&server, "broke").await,
        State {
            state: "healthy".into(),
            score: 0,
            since_sha: None,
            failure_hash: None
        }
    );
    assert!(events(&server).await.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn the_broken_streak_length_is_configurable() {
    let Some(server) = start(&[("ASTER_SRV_BROKEN_RUNS", "4")]).await else {
        return;
    };
    let now = Utc::now();
    for (i, minutes) in [100, 90, 80].into_iter().enumerate() {
        let id = format!("m{i}");
        seed(
            &server,
            &Run::new(&id, "main", ago(now, minutes))
                .hash(&id)
                .result("t", "fail", Some("same")),
        )
        .await;
    }
    classify(&server, now).await;
    assert_eq!(state(&server, "t").await.state, "healthy");
    seed(
        &server,
        &Run::new("m3", "main", ago(now, 5))
            .hash("m3")
            .result("t", "fail", Some("same")),
    )
    .await;
    classify(&server, now).await;
    assert_eq!(state(&server, "t").await.state, "broken");
    server.stop().await;
}

#[tokio::test]
async fn partial_failures_never_break_a_test() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    for (i, minutes) in [100, 90, 80].into_iter().enumerate() {
        let id = format!("m{i}");
        let mut run =
            Run::new(&id, "main", ago(now, minutes))
                .hash(&id)
                .result("t", "fail", Some("same"));
        run.confidence = Some("partial");
        seed(&server, &run).await;
    }
    classify(&server, now).await;
    assert_eq!(state(&server, "t").await.state, "healthy");
    server.stop().await;
}

#[tokio::test]
async fn running_again_changes_nothing() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(
        &server,
        &Run::new("r1", "main", ago(now, 50))
            .result("flip", "pass", None)
            .result("blip", "fail", Some("f"))
            .result("dead", "fail", Some("d")),
    )
    .await;
    seed(
        &server,
        &Run::new("r2", "main", ago(now, 40))
            .result("flip", "fail", Some("f"))
            .result("blip", "pass", None)
            .result("dead", "fail", Some("d")),
    )
    .await;
    seed(
        &server,
        &Run::new("r3", "main", ago(now, 30))
            .hash("h9")
            .result("dead", "fail", Some("d"))
            .attempt("retry", "fail", 1, Some("f"))
            .attempt("retry", "pass", 2, None),
    )
    .await;

    let first = classify(&server, now).await;
    assert!(first.events > 0);
    let events_before = events(&server).await;
    let tests_before: Vec<_> = {
        let mut all = Vec::new();
        for test in ["flip", "blip", "dead", "retry"] {
            all.push(state(&server, test).await);
        }
        all
    };
    assert_eq!(tests_before[2].state, "broken");

    // A second pass finds nothing new.
    let second = classify(&server, now).await;
    assert_eq!((second.events, second.tests_updated), (0, 0));
    // Even starting over from the beginning writes nothing new.
    server
        .client()
        .await
        .execute("DELETE FROM test_results.classifier_state", &[])
        .await
        .unwrap();
    let third = classify(&server, now).await;
    assert_eq!((third.events, third.tests_updated), (0, 0));
    assert_eq!(events(&server).await, events_before);
    for (i, test) in ["flip", "blip", "dead", "retry"].into_iter().enumerate() {
        assert_eq!(state(&server, test).await, tests_before[i], "{test}");
    }
    server.stop().await;
}

#[tokio::test]
async fn the_score_counts_seven_days_and_decays() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    // One definite (3) and one likely (1) event.
    seed(
        &server,
        &Run::new("r1", "pr-1", ago(now, 50)).result("t", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r2", "pr-1", ago(now, 40)).result("t", "fail", Some("f")),
    )
    .await;
    seed(
        &server,
        &Run::new("m1", "main", ago(now, 30))
            .hash("a")
            .result("t", "fail", Some("f")),
    )
    .await;
    seed(
        &server,
        &Run::new("m2", "main", ago(now, 20))
            .hash("b")
            .result("t", "pass", None),
    )
    .await;
    classify(&server, now).await;
    assert_eq!(
        events(&server).await,
        vec![
            event("t", "definite", "input_hash", "r2"),
            event("t", "likely", "next_main_run", "m1"),
        ]
    );
    // r2 (fail, h1) vs r1 (pass, h1) is definite; m1 (fail, h=a) is the
    // first run of its hash.
    assert_eq!(state(&server, "t").await.score, 4);

    // Six days on, both still count.
    let later = now + Duration::days(6);
    classify(&server, later).await;
    let still = state(&server, "t").await;
    assert_eq!((still.state.as_str(), still.score), ("flaky", 4));

    // Seven days after each failure its event ages out: r2's (40 minutes
    // ago) first, then m1's (30 minutes ago).
    let later = now + Duration::days(7) - Duration::minutes(35);
    classify(&server, later).await;
    assert_eq!(state(&server, "t").await.score, 1);
    let later = now + Duration::days(8);
    classify(&server, later).await;
    let aged = state(&server, "t").await;
    assert_eq!((aged.state.as_str(), aged.score), ("healthy", 0));
    let last: Option<DateTime<Utc>> = server
        .client()
        .await
        .query_one(
            "SELECT last_flake_at FROM test_results.tests WHERE test_id = 't'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(last.is_some(), "the last flake is remembered");
    server.stop().await;
}

#[tokio::test]
async fn one_replica_classifies_at_a_time() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let holder = server.client().await;
    // The classifier's lock, "aster-cl".
    holder
        .execute("SELECT pg_advisory_lock($1)", &[&0x6173_7465_722d_636c_i64])
        .await
        .unwrap();
    assert_eq!(
        classifier::run_once(&server.app, Utc::now()).await.unwrap(),
        None
    );
    holder
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&0x6173_7465_722d_636c_i64],
        )
        .await
        .unwrap();
    assert!(classifier::run_once(&server.app, Utc::now())
        .await
        .unwrap()
        .is_some());
    server.stop().await;
}

#[tokio::test]
async fn failure_clusters_group_failures_by_hash() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(
        &server,
        &Run::new("r1", "main", ago(now, 20))
            .result("a", "fail", Some("f1"))
            .result("b", "fail", Some("f1"))
            .result("c", "error", Some("f2"))
            .result("d", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r2", "main", ago(now, 10)).result("a", "fail", Some("f1")),
    )
    .await;
    let rows = server
        .client()
        .await
        .query(
            "SELECT failure_hash, failures, tests, example_message FROM test_results.failure_clusters \
             ORDER BY failure_hash",
            &[],
        )
        .await
        .unwrap();
    let rows: Vec<(String, i64, i64, Option<String>)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0.as_str(), rows[0].1, rows[0].2), ("f1", 3, 2));
    assert!(rows[0].3.as_deref().unwrap().starts_with("boom in"));
    assert_eq!((rows[1].0.as_str(), rows[1].1, rows[1].2), ("f2", 1, 1));
    server.stop().await;
}

#[tokio::test]
async fn one_failure_between_two_passes_is_one_flake_and_two_failures_are_two() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    // Pass, fail, pass on one hash: comparing each run with the one before
    // finds the failure twice; it is still one failure.
    for (run, at, outcome) in [("a1", 50, "pass"), ("a2", 40, "fail"), ("a3", 30, "pass")] {
        let failure = (outcome == "fail").then_some("f1");
        seed(
            &server,
            &Run::new(run, "pr-1", ago(now, at)).result("once", outcome, failure),
        )
        .await;
    }
    // Fail, pass, fail: two failures, two events.
    for (run, at, outcome) in [("b1", 45, "fail"), ("b2", 35, "pass"), ("b3", 25, "fail")] {
        let failure = (outcome == "fail").then_some("f1");
        seed(
            &server,
            &Run::new(run, "pr-2", ago(now, at))
                .hash("h2")
                .result("twice", outcome, failure),
        )
        .await;
    }
    classify(&server, now).await;
    assert_eq!(
        events(&server).await,
        vec![
            event("once", "definite", "input_hash", "a2"),
            event("twice", "definite", "input_hash", "b1"),
            event("twice", "definite", "input_hash", "b3"),
        ]
    );
    assert_eq!(state(&server, "once").await.score, 3);
    assert_eq!(state(&server, "twice").await.score, 6);
    server.stop().await;
}

#[tokio::test]
async fn a_definite_event_found_later_replaces_the_likely_one() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    // A main failure, then a main pass on other inputs: likely, for m1.
    seed(
        &server,
        &Run::new("m1", "main", ago(now, 50))
            .hash("a")
            .result("t", "fail", Some("f1")),
    )
    .await;
    seed(
        &server,
        &Run::new("m2", "main", ago(now, 40))
            .hash("b")
            .result("t", "pass", None),
    )
    .await;
    classify(&server, ago(now, 35)).await;
    assert_eq!(
        events(&server).await,
        vec![event("t", "likely", "next_main_run", "m1")]
    );
    assert_eq!(state(&server, "t").await.score, 1);
    // A later run on m1's inputs passes: m1's failure is now proven a
    // flake, and it is still one failure.
    seed(
        &server,
        &Run::new("p1", "pr-1", ago(now, 20))
            .hash("a")
            .result("t", "pass", None),
    )
    .await;
    classify(&server, now).await;
    assert_eq!(
        events(&server).await,
        vec![event("t", "definite", "input_hash", "m1")]
    );
    assert_eq!(state(&server, "t").await.score, 3);
    server.stop().await;
}

/// A run of `tests` tests whose first `failing` fail with one shared
/// failure hash and the rest pass.
fn shared_failure<'a>(
    run: &'a str,
    at: DateTime<Utc>,
    names: &'a [String],
    failing: usize,
) -> Run<'a> {
    let mut seeded = Run::new(run, "pr-1", at);
    for (i, name) in names.iter().enumerate() {
        seeded = if i < failing {
            seeded.result(name, "fail", Some("db down"))
        } else {
            seeded.result(name, "pass", None)
        };
    }
    seeded
}

#[tokio::test]
async fn ten_failures_sharing_one_hash_are_an_incident_and_nine_are_flakes() {
    let names: Vec<String> = (0..100).map(|i| format!("t{i:02}")).collect();
    for (failing, incident) in [(10, true), (9, false)] {
        let Some(server) = start(&[]).await else {
            return;
        };
        let now = Utc::now();
        seed(&server, &shared_failure("r1", ago(now, 30), &names, 0)).await;
        seed(
            &server,
            &shared_failure("r2", ago(now, 20), &names, failing),
        )
        .await;
        let report = classify(&server, now).await;
        let flaky = server
            .client()
            .await
            .query_one(
                "SELECT count(*) FROM test_results.tests WHERE state = 'flaky'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        if incident {
            assert_eq!((report.incidents, report.events, flaky), (1, 0, 0));
        } else {
            assert_eq!((report.incidents, report.events, flaky), (0, 9, 9));
        }
        server.stop().await;
    }
}
