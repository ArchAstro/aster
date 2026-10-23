//! Quarantine and webhook notifications against a real Postgres: runs are
//! seeded at chosen times, the classifier runs with a chosen clock, and the
//! feed and the deliveries are read back.

mod common;

use std::sync::{Arc, Mutex};

use aster_srv::test_results::classifier;
use aster_srv::webhooks;
use chrono::{DateTime, Duration, Utc};
use common::{seed, start, Run, Server, REPO};
use serde_json::Value;

fn ago(now: DateTime<Utc>, minutes: i64) -> DateTime<Utc> {
    now - Duration::minutes(minutes)
}

async fn classify(server: &Server, now: DateTime<Utc>) -> classifier::Report {
    classifier::run_once(&server.app, now)
        .await
        .unwrap()
        .expect("the lock is free")
}

/// A run in which `test` failed and then passed on its retry: one definite
/// flake event.
fn retried<'a>(run: &'a str, test: &'a str, at: DateTime<Utc>) -> Run<'a> {
    Run::new(run, "pr-1", at)
        .attempt(test, "fail", 1, Some("f1"))
        .attempt(test, "pass", 2, None)
}

async fn feed(server: &Server) -> Value {
    let (status, body) = server.get("/v1/test-results/quarantine", Some(REPO)).await;
    assert_eq!(status, 200, "{body}");
    body
}

fn quarantined(feed: &Value) -> Vec<String> {
    feed["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["test_id"].as_str().unwrap().to_string())
        .collect()
}

/// `(test, action)` of every log row, oldest first.
async fn log(server: &Server) -> Vec<(String, String)> {
    server
        .client()
        .await
        .query(
            "SELECT test_id, action FROM test_results.quarantine_log ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

fn entry(test: &str, action: &str) -> (String, String) {
    (test.into(), action.into())
}

#[tokio::test]
async fn definite_flakes_in_two_runs_quarantine_a_test() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "twice", ago(now, 60))).await;
    seed(&server, &retried("r2", "twice", ago(now, 50))).await;
    // Two definite events, but from one run.
    seed(
        &server,
        &Run::new("r3", "pr-2", ago(now, 45)).result("once", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r4", "pr-2", ago(now, 40))
            .attempt("once", "fail", 1, Some("f1"))
            .attempt("once", "pass", 2, None),
    )
    .await;
    let empty = feed(&server).await;
    assert_eq!(empty["version"], 0);
    assert!(quarantined(&empty).is_empty());

    let report = classify(&server, now).await;
    assert_eq!(report.quarantine.quarantined, 1);

    let list = feed(&server).await;
    assert_eq!(quarantined(&list), vec!["twice"]);
    assert_eq!(list["repo"], REPO);
    assert_eq!(list["truncated"], false);
    let test = &list["tests"][0];
    assert_eq!(test["target"], "//a:test");
    assert_eq!(test["file"], "test/twice_test.exs");
    assert_eq!(test["runs"], 2);
    let since: DateTime<Utc> = test["since"].as_str().unwrap().parse().unwrap();
    let expires: DateTime<Utc> = test["expires_at"].as_str().unwrap().parse().unwrap();
    assert_eq!((expires - since).num_days(), 30);
    assert!(list["version"].as_i64().unwrap() > 0);
    assert_eq!(log(&server).await, vec![entry("twice", "quarantined")]);

    // The list filter shows the same tests.
    let (_, page) = server
        .get("/v1/test-results/tests?state=quarantined", Some(REPO))
        .await;
    let listed: Vec<&str> = page["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["test_id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["twice"]);

    // Running again changes nothing and keeps the version.
    let again = classify(&server, now + Duration::minutes(5)).await;
    assert_eq!(again.quarantine, Default::default());
    assert_eq!(feed(&server).await["version"], list["version"]);
    server.stop().await;
}

#[tokio::test]
async fn events_older_than_seven_days_do_not_count() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "slow", ago(now, 8 * 24 * 60))).await;
    seed(&server, &retried("r2", "slow", ago(now, 10))).await;
    classify(&server, now).await;
    assert!(quarantined(&feed(&server).await).is_empty());
    server.stop().await;
}

#[tokio::test]
async fn the_entry_threshold_is_configurable_and_zero_turns_it_off() {
    let Some(server) = start(&[("ASTER_SRV_QUARANTINE_RUNS", "1")]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "one", ago(now, 10))).await;
    classify(&server, now).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["one"]);
    server.stop().await;

    let Some(server) = start(&[("ASTER_SRV_QUARANTINE_RUNS", "0")]).await else {
        return;
    };
    seed(&server, &retried("r1", "one", ago(now, 20))).await;
    seed(&server, &retried("r2", "one", ago(now, 10))).await;
    classify(&server, now).await;
    assert!(quarantined(&feed(&server).await).is_empty());
    server.stop().await;
}

/// Three default-branch failures with one hash after `minutes` ago: the
/// test is broken.
async fn break_on_main(server: &Server, test: &str, now: DateTime<Utc>, minutes: i64) {
    for (index, run) in ["m1", "m2", "m3"].into_iter().enumerate() {
        seed(
            server,
            &Run::new(run, "main", ago(now, minutes - index as i64))
                .hash(run)
                .result(test, "fail", Some("same")),
        )
        .await;
    }
}

#[tokio::test]
async fn broken_tests_are_never_quarantined_and_leave_when_they_break() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    // Proven flaky, and also broken on main by now.
    seed(&server, &retried("r1", "both", ago(now, 60))).await;
    seed(&server, &retried("r2", "both", ago(now, 50))).await;
    break_on_main(&server, "both", now, 30).await;
    classify(&server, now).await;
    assert!(quarantined(&feed(&server).await).is_empty());
    server.stop().await;

    // Quarantined first, broken later.
    let Some(server) = start(&[]).await else {
        return;
    };
    seed(&server, &retried("r1", "later", ago(now, 60))).await;
    seed(&server, &retried("r2", "later", ago(now, 50))).await;
    classify(&server, ago(now, 40)).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["later"]);
    break_on_main(&server, "later", now, 30).await;
    let report = classify(&server, now).await;
    assert_eq!(report.quarantine.broken, 1);
    assert!(quarantined(&feed(&server).await).is_empty());
    assert_eq!(
        log(&server).await,
        vec![entry("later", "quarantined"), entry("later", "broken")]
    );
    server.stop().await;
}

#[tokio::test]
async fn consecutive_passes_release_and_a_release_starts_afresh() {
    let Some(server) = start(&[("ASTER_SRV_QUARANTINE_RELEASE_PASSES", "3")]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "t", ago(now, 100))).await;
    seed(&server, &retried("r2", "t", ago(now, 90))).await;
    classify(&server, ago(now, 80)).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);

    // pass, pass, fail, pass, pass: the fail resets the count.
    for (run, minutes, outcome) in [
        ("p1", 70, "pass"),
        ("p2", 65, "pass"),
        ("p3", 60, "fail"),
        ("p4", 55, "pass"),
        ("p5", 50, "pass"),
    ] {
        let failure = (outcome == "fail").then_some("f9");
        seed(
            &server,
            &Run::new(run, "pr-9", ago(now, minutes))
                .hash(run)
                .result("t", outcome, failure),
        )
        .await;
    }
    classify(&server, ago(now, 45)).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);

    seed(
        &server,
        &Run::new("p6", "pr-9", ago(now, 40))
            .hash("p6")
            .result("t", "pass", None),
    )
    .await;
    let report = classify(&server, ago(now, 35)).await;
    assert_eq!(report.quarantine.released, 1);
    assert!(quarantined(&feed(&server).await).is_empty());

    // The events that quarantined it are still within 7 days, but only
    // events after the release count.
    classify(&server, ago(now, 30)).await;
    assert!(quarantined(&feed(&server).await).is_empty());
    seed(&server, &retried("r3", "t", ago(now, 20))).await;
    seed(&server, &retried("r4", "t", ago(now, 10))).await;
    classify(&server, now).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);
    assert_eq!(
        log(&server).await,
        vec![
            entry("t", "quarantined"),
            entry("t", "released"),
            entry("t", "quarantined")
        ]
    );
    server.stop().await;
}

#[tokio::test]
async fn replayed_passes_from_cached_targets_do_not_release() {
    let Some(server) = start(&[("ASTER_SRV_QUARANTINE_RELEASE_PASSES", "2")]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "t", ago(now, 100))).await;
    seed(&server, &retried("r2", "t", ago(now, 90))).await;
    classify(&server, ago(now, 80)).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);

    for (run, minutes) in [("c1", 70), ("c2", 65)] {
        let mut replayed = Run::new(run, "pr-9", ago(now, minutes)).result("t", "pass", None);
        replayed.cached = true;
        seed(&server, &replayed).await;
    }
    assert_eq!(classify(&server, ago(now, 60)).await.quarantine.released, 0);
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);

    for (run, minutes) in [("p1", 55), ("p2", 50)] {
        seed(
            &server,
            &Run::new(run, "pr-9", ago(now, minutes)).result("t", "pass", None),
        )
        .await;
    }
    assert_eq!(classify(&server, ago(now, 45)).await.quarantine.released, 1);
    server.stop().await;
}

#[tokio::test]
async fn entries_expire() {
    let Some(server) = start(&[("ASTER_SRV_QUARANTINE_DAYS", "1")]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "t", ago(now, 30))).await;
    seed(&server, &retried("r2", "t", ago(now, 20))).await;
    classify(&server, ago(now, 10)).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);
    let report = classify(&server, now + Duration::hours(24)).await;
    assert_eq!(report.quarantine.expired, 1);
    assert!(quarantined(&feed(&server).await).is_empty());
    server.stop().await;
}

#[tokio::test]
async fn a_repository_reads_only_its_own_list() {
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "t", ago(now, 20))).await;
    seed(&server, &retried("r2", "t", ago(now, 10))).await;
    classify(&server, now).await;
    let (status, other) = server
        .get("/v1/test-results/quarantine", Some("ArchAstro/other"))
        .await;
    assert_eq!(status, 200);
    assert!(quarantined(&other).is_empty());
    assert_eq!(other["version"], 0);
    let (status, _) = server
        .get(
            "/v1/test-results/quarantine?repo=ArchAstro%2Ffirstlanding",
            Some("ArchAstro/other"),
        )
        .await;
    assert_eq!(status, 403);
    let (status, _) = server.get("/v1/test-results/quarantine", None).await;
    assert_eq!(status, 401);
    let (status, _) = server
        .get("/v1/test-results/quarantine?limit=5", Some(REPO))
        .await;
    assert_eq!(status, 400);
    server.stop().await;
}

/// A webhook receiver on this machine: every request's headers and body,
/// answered with `status`.
struct Receiver {
    url: String,
    received: Arc<Mutex<Vec<(hyper::HeaderMap, String)>>>,
}

async fn receiver(status: u16) -> Receiver {
    use http_body_util::BodyExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::clone(&received);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |request: hyper::Request<hyper::body::Incoming>| {
                        let store = Arc::clone(&store);
                        async move {
                            let headers = request.headers().clone();
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            store
                                .lock()
                                .unwrap()
                                .push((headers, String::from_utf8(body.to_vec()).unwrap()));
                            let mut response = hyper::Response::new(http_body_util::Full::new(
                                bytes::Bytes::new(),
                            ));
                            *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                            Ok::<_, std::convert::Infallible>(response)
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Receiver { url, received }
}

const SECRET: &str = "a-test-secret-0123456789";

fn webhooks_env(url: &str, events: &str) -> String {
    format!(r#"[{{"name": "relay", "url": "{url}", "secret": "{SECRET}", "events": [{events}]}}]"#)
}

async fn deliveries(server: &Server) -> Vec<(String, i32, bool, bool)> {
    server
        .client()
        .await
        .query(
            "SELECT event, attempts, delivered_at IS NOT NULL, abandoned_at IS NOT NULL \
             FROM webhooks.deliveries ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
}

#[tokio::test]
async fn subscribed_endpoints_get_signed_notifications() {
    let hook = receiver(204).await;
    let config = webhooks_env(&hook.url, r#""test.quarantined", "test.broken""#);
    let Some(server) = start(&[("ASTER_SRV_WEBHOOKS", config.as_str())]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "flaky", ago(now, 60))).await;
    seed(&server, &retried("r2", "flaky", ago(now, 50))).await;
    break_on_main(&server, "gone", now, 40).await;
    let report = classify(&server, now).await;
    assert_eq!(report.deliveries, 2);
    // Nothing is sent until the sender runs.
    assert!(hook.received.lock().unwrap().is_empty());

    let sent = webhooks::deliver_once(&server.app, now).await.unwrap();
    assert_eq!(sent.delivered, 2);
    let received = hook.received.lock().unwrap().clone();
    let mut events: Vec<(String, Value)> = received
        .iter()
        .map(|(headers, body)| {
            let event = headers["x-aster-event"].to_str().unwrap().to_string();
            assert_eq!(
                headers["x-aster-signature"].to_str().unwrap(),
                webhooks::signature(SECRET, body.as_bytes())
            );
            assert!(headers.contains_key("x-aster-delivery"));
            (event, serde_json::from_str(body).unwrap())
        })
        .collect();
    events.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(events[0].0, "test.broken");
    assert_eq!(events[0].1["test"]["test_id"], "gone");
    assert_eq!(events[0].1["detail"]["since_sha"], "m1");
    assert_eq!(events[0].1["repo"], REPO);
    assert_eq!(events[1].0, "test.quarantined");
    assert_eq!(events[1].1["event"], "test.quarantined");
    assert_eq!(events[1].1["test"]["name"], "flaky");
    assert_eq!(events[1].1["detail"]["runs"], 2);

    // Delivered rows are not sent again.
    let again = webhooks::deliver_once(&server.app, now + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(again, Default::default());
    assert_eq!(hook.received.lock().unwrap().len(), 2);
    server.stop().await;
}

#[tokio::test]
async fn a_failing_endpoint_is_retried_with_backoff_and_blocks_nothing() {
    let hook = receiver(500).await;
    let config = webhooks_env(&hook.url, r#""test.quarantined""#);
    let Some(server) = start(&[("ASTER_SRV_WEBHOOKS", config.as_str())]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "t", ago(now, 60))).await;
    seed(&server, &retried("r2", "t", ago(now, 50))).await;
    classify(&server, now).await;
    assert_eq!(quarantined(&feed(&server).await), vec!["t"]);

    let first = webhooks::deliver_once(&server.app, now).await.unwrap();
    assert_eq!(first.retrying, 1);
    // Not due again until the backoff has passed.
    let early = webhooks::deliver_once(&server.app, now + Duration::seconds(10))
        .await
        .unwrap();
    assert_eq!(early, Default::default());
    let second = webhooks::deliver_once(&server.app, now + Duration::seconds(31))
        .await
        .unwrap();
    assert_eq!(second.retrying, 1);
    assert_eq!(
        deliveries(&server).await,
        vec![("test.quarantined".into(), 2, false, false)]
    );
    let row = server
        .client()
        .await
        .query_one(
            "SELECT last_status, last_error FROM webhooks.deliveries",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Option<i32>>(0), Some(500));
    assert!(row.get::<_, String>(1).contains("500"));
    assert_eq!(hook.received.lock().unwrap().len(), 2);

    // After the last attempt the delivery is abandoned.
    let mut at = now;
    for _ in 0..20 {
        at += Duration::hours(7);
        webhooks::deliver_once(&server.app, at).await.unwrap();
    }
    assert_eq!(
        deliveries(&server).await,
        vec![("test.quarantined".into(), 12, false, true)]
    );
    server.stop().await;
}

#[tokio::test]
async fn unsubscribed_or_unconfigured_events_record_nothing() {
    let hook = receiver(204).await;
    let config = webhooks_env(&hook.url, r#""test.released""#);
    let Some(server) = start(&[("ASTER_SRV_WEBHOOKS", config.as_str())]).await else {
        return;
    };
    let now = Utc::now();
    seed(&server, &retried("r1", "t", ago(now, 60))).await;
    seed(&server, &retried("r2", "t", ago(now, 50))).await;
    assert_eq!(classify(&server, now).await.deliveries, 0);
    server.stop().await;

    let Some(server) = start(&[]).await else {
        return;
    };
    seed(&server, &retried("r1", "t", ago(now, 60))).await;
    seed(&server, &retried("r2", "t", ago(now, 50))).await;
    assert_eq!(classify(&server, now).await.deliveries, 0);
    assert!(deliveries(&server).await.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn a_pass_behind_the_committed_watermark_changes_and_announces_nothing() {
    let hook = receiver(204).await;
    let config = webhooks_env(&hook.url, r#""test.broken""#);
    let Some(server) = start(&[("ASTER_SRV_WEBHOOKS", config.as_str())]).await else {
        return;
    };
    let now = Utc::now();
    break_on_main(&server, "gone", now, 30).await;
    assert_eq!(classify(&server, now).await.deliveries, 1);
    // A replica whose clock is behind: as of 29 minutes ago the test was
    // not broken yet. Re-deriving that would undo the state and announce
    // the break a second time on the next pass.
    assert_eq!(classify(&server, ago(now, 29)).await, Default::default());
    let next = classify(&server, now + Duration::minutes(5)).await;
    assert_eq!((next.tests_updated, next.deliveries), (0, 0));
    assert_eq!(
        deliveries(&server).await,
        vec![("test.broken".into(), 0, false, false)]
    );
    server.stop().await;
}

#[tokio::test]
async fn a_run_flaking_more_tests_than_the_cap_quarantines_none_of_them() {
    // Three tests flake in the same two runs. Below the incident thresholds
    // (three failures of ten tests), so each is a definite flake.
    let tests = ["a", "b", "c"];
    let run = |id: &'static str, at| {
        let mut seeded = Run::new(id, "pr-1", at);
        for test in tests {
            seeded = seeded
                .attempt(test, "fail", 1, Some("f1"))
                .attempt(test, "pass", 2, None);
        }
        for test in ["d", "e", "f", "g", "h", "i", "j"] {
            seeded = seeded.result(test, "pass", None);
        }
        seeded
    };
    for (cap, want) in [("2", vec![]), ("3", vec!["a", "b", "c"])] {
        let Some(server) = start(&[("ASTER_SRV_QUARANTINE_RUN_CAP", cap)]).await else {
            return;
        };
        let now = Utc::now();
        seed(&server, &run("r1", ago(now, 20))).await;
        seed(&server, &run("r2", ago(now, 10))).await;
        classify(&server, now).await;
        let mut got = quarantined(&feed(&server).await);
        got.sort();
        assert_eq!(got, want, "cap {cap}");
        // Either way each test is still reported flaky.
        let flaky = server
            .client()
            .await
            .query_one(
                "SELECT count(*) FROM test_results.tests WHERE flake_score = 6",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        assert_eq!(flaky, 3, "cap {cap}");
        server.stop().await;
    }
}
