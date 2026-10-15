//! The dashboard (`GET /ui/...`) against a real Postgres, with IAP
//! assertions signed by a fixture key.

mod common;

use chrono::{Duration as Span, Utc};
use common::{iap_assertion, iap_assertion_for, seed, start, Run, Server, IAP_AUDIENCE, REPO};

const OTHER: &str = "ArchAstro/other";
const AUDIENCE_SETTING: (&str, &str) = ("ASTER_SRV_IAP_AUDIENCE", IAP_AUDIENCE);

async fn page(server: &Server, path: &str) -> (u16, String) {
    let (status, _, body) = server
        .get_page(path, Some(&iap_assertion("ann@example.com")))
        .await;
    (status, body)
}

/// Runs of `REPO` for three days, with one flaky, one broken and one
/// failing test, and a run of `OTHER` that is newer.
async fn seeded() -> Option<Server> {
    let server = start(&[AUDIENCE_SETTING]).await?;
    let now = Utc::now();
    let hours = |h: i64| now - Span::hours(h);
    seed(
        &server,
        &Run::new("r1", "main", hours(30))
            .result("flaky <test>", "pass", None)
            .result("broken/one", "fail", Some("hash-b"))
            .result("steady", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r2", "main", hours(20))
            .attempt("flaky <test>", "fail", 1, Some("hash-a"))
            .attempt("flaky <test>", "pass", 2, None)
            .result("broken/one", "fail", Some("hash-b"))
            .result("steady", "pass", None),
    )
    .await;
    seed(
        &server,
        &Run::new("r3", "feat/x", hours(10))
            .result("flaky <test>", "fail", Some("hash-a"))
            .result("broken/one", "fail", Some("hash-b"))
            .result("steady", "pass", None),
    )
    .await;
    // Failures older than the clusters window do not count.
    seed(
        &server,
        &Run::new("old", "main", now - Span::days(12)).result("steady", "fail", Some("hash-old")),
    )
    .await;
    let mut other = Run::new("o1", "main", hours(1)).result("z", "pass", None);
    other.repo = OTHER;
    seed(&server, &other).await;

    let client = server.client().await;
    client
        .batch_execute(
            "UPDATE test_results.tests SET state = 'flaky', flake_score = 42, \
                 last_flake_at = now() WHERE test_id = 'flaky <test>'; \
             UPDATE test_results.tests SET state = 'broken', flake_score = 3, \
                 broken_since_sha = 'abcdef0123456789' WHERE test_id = 'broken/one'; \
             UPDATE test_results.test_results SET message = '<script>alert(1)</script>' \
                 WHERE run_id = 'r1' AND test_id = 'broken/one'; \
             UPDATE test_results.test_results SET message = repeat('long line ', 40) \
                 WHERE run_id = 'r3' AND test_id = 'broken/one'",
        )
        .await
        .unwrap();
    drop(client);
    Some(server)
}

#[tokio::test]
async fn the_leaderboard_lists_flaky_and_broken_tests_for_the_newest_repository() {
    let Some(server) = seeded().await else {
        return;
    };
    let (status, headers, body) = server
        .get_page("/ui", Some(&iap_assertion("ann@example.com")))
        .await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(
        headers["content-security-policy"],
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "same-origin");
    assert_eq!(headers["x-frame-options"], "DENY");
    // OTHER has the newest run, so it is the default: no tests, but a switcher.
    assert!(body.contains("ann@example.com"));
    assert!(body.contains(OTHER) && body.contains(REPO));
    assert!(body.contains("No flaky tests"));

    let (status, body) = page(&server, "/ui?repo=ArchAstro%2Ffirstlanding").await;
    assert_eq!(status, 200);
    assert!(body.contains("flaky &lt;test&gt;"), "{body}");
    assert!(!body.contains("flaky <test>"));
    assert!(body.contains(">42<"));
    assert!(body.contains("/ui/tests/flaky%20%3Ctest%3E?repo=ArchAstro%2Ffirstlanding"));
    // The broken section names the commit it began at.
    assert!(body.contains("<h2>Broken</h2>") && body.contains("abcdef0123"));
    assert!(body.contains("/ui/tests/broken%2Fone?repo=ArchAstro%2Ffirstlanding"));
    assert!(!body.contains("steady"));
    // The repo parameter is case-insensitive and nav links keep it.
    let (_, lower) = page(&server, "/ui/clusters?repo=archastro/firstlanding").await;
    assert!(lower.contains("href=\"/ui?repo=ArchAstro%2Ffirstlanding\""));
    server.stop().await;
}

#[tokio::test]
async fn a_test_page_shows_its_strip_and_escaped_messages() {
    let Some(server) = seeded().await else {
        return;
    };
    let (status, body) = page(
        &server,
        "/ui/tests/broken%2Fone?repo=ArchAstro%2Ffirstlanding",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("abcdef0123456789"));
    assert_eq!(body.matches("class=\"cell fail\"").count(), 3, "{body}");
    assert!(body.contains("oldest on the left"));
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!body.contains("<script>"));
    // A long message is collapsed.
    assert!(body.contains("<details>") && body.contains("long line long line"));

    // Attempts of one run are separate cells: fail, then pass.
    let (_, flaky) = page(
        &server,
        "/ui/tests/flaky%20%3Ctest%3E?repo=ArchAstro%2Ffirstlanding",
    )
    .await;
    assert_eq!(flaky.matches("class=\"cell pass\"").count(), 2);
    assert_eq!(flaky.matches("class=\"cell fail\"").count(), 2);
    assert!(flaky.contains("attempt 2"));

    let (status, body) = page(&server, "/ui/tests/nope?repo=ArchAstro%2Ffirstlanding").await;
    assert_eq!(status, 404);
    assert!(body.contains("No such test"));
    // A test of another repository is not found under this one.
    let (status, _) = page(&server, "/ui/tests/broken%2Fone?repo=ArchAstro%2Fother").await;
    assert_eq!(status, 404);
    server.stop().await;
}

#[tokio::test]
async fn clusters_group_recent_failures_by_hash() {
    let Some(server) = seeded().await else {
        return;
    };
    let (status, body) = page(&server, "/ui/clusters?repo=ArchAstro%2Ffirstlanding").await;
    assert_eq!(status, 200);
    // hash-b: three failures of one test; hash-a: two of one; hash-old is too old.
    assert!(body.contains("<code>hash-b</code>"), "{body}");
    assert!(body.contains("<code>hash-a</code>"));
    assert!(!body.contains("hash-old"));
    let b = body.find("hash-b").unwrap();
    let a = body.find("hash-a").unwrap();
    assert!(b < a, "the larger cluster comes first");
    assert!(body.contains("<td class=\"num\">3</td><td class=\"num\">1</td><td><code>hash-b"));
    assert!(body.contains("/ui/tests/broken%2Fone?repo=ArchAstro%2Ffirstlanding"));

    // The index that keeps this query to the failing rows exists.
    let exists = server
        .client()
        .await
        .query_opt(
            "SELECT 1 FROM pg_indexes WHERE indexname = 'test_results_failures'",
            &[],
        )
        .await
        .unwrap();
    assert!(exists.is_some());
    server.stop().await;
}

#[tokio::test]
async fn the_quarantine_page_says_it_is_not_enabled() {
    let Some(server) = seeded().await else {
        return;
    };
    let (status, body) = page(&server, "/ui/quarantine?repo=ArchAstro%2Ffirstlanding").await;
    assert_eq!(status, 200);
    assert!(body.contains("Quarantine is not enabled yet"));
    server.stop().await;
}

#[tokio::test]
async fn an_empty_database_still_renders() {
    let Some(server) = start(&[AUDIENCE_SETTING]).await else {
        return;
    };
    for path in ["/ui", "/ui/clusters", "/ui/quarantine", "/ui/tests/x"] {
        let (status, body) = page(&server, path).await;
        assert_eq!(status, 200, "{path}");
        assert!(body.contains("ann@example.com"), "{path}");
    }
    server.stop().await;
}

#[tokio::test]
async fn refuses_anyone_who_did_not_pass_iap() {
    let Some(server) = seeded().await else {
        return;
    };
    let (status, _, body) = server.get_page("/ui", None).await;
    assert_eq!(status, 401);
    assert!(body.contains("sign in through IAP"));
    let (status, _, _) = server.get_page("/ui", Some("garbage")).await;
    assert_eq!(status, 401);
    let wrong = iap_assertion_for("ann@example.com", "/projects/999/global/backendServices/1");
    let (status, _, _) = server.get_page("/ui", Some(&wrong)).await;
    assert_eq!(status, 401);
    // A GitHub OIDC bearer token is not an IAP assertion.
    let (status, _, _) = server
        .get_page("/ui", Some(&common::token("ArchAstro/firstlanding")))
        .await;
    assert_eq!(status, 401);

    let ok = iap_assertion("ann@example.com");
    // Repositories off the allowlist are not viewable.
    let (status, _, _) = server.get_page("/ui?repo=evil%2Frepo", Some(&ok)).await;
    assert_eq!(status, 403);
    let (status, _, _) = server.get_page("/ui?repo=%zz", Some(&ok)).await;
    assert_eq!(status, 400);
    let (status, _, _) = server.get_page("/ui/nowhere", Some(&ok)).await;
    assert_eq!(status, 404);
    let (status, _, _) = server.get_page("/ui/tests/", Some(&ok)).await;
    assert_eq!(status, 404);

    let response = aster_srv::http_client()
        .build()
        .unwrap()
        .post(format!("{}/ui", server.url))
        .header("x-goog-iap-jwt-assertion", &ok)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    // The JSON API is untouched by the dashboard's auth.
    let (status, _) = server.get("/v1/test-results/tests", None).await;
    assert_eq!(status, 401);
    server.stop().await;
}

#[tokio::test]
async fn the_email_domain_filter_keeps_service_accounts_out() {
    let Some(server) = start(&[
        AUDIENCE_SETTING,
        ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
    ])
    .await
    else {
        return;
    };
    let (status, _) = page(&server, "/ui").await;
    assert_eq!(status, 200);
    let service = iap_assertion("uploader@proj.iam.gserviceaccount.com");
    let (status, _, body) = server.get_page("/ui", Some(&service)).await;
    assert_eq!(status, 403);
    assert!(body.contains("may not view this dashboard"));
    server.stop().await;
}

#[tokio::test]
async fn the_dashboard_is_off_without_an_audience_and_trusts_the_dev_viewer_when_asked() {
    let Some(off) = start(&[]).await else {
        return;
    };
    let (status, _, _) = off
        .get_page("/ui", Some(&iap_assertion("ann@example.com")))
        .await;
    assert_eq!(status, 404);
    let (status, _, _) = off.get_page("/ui/clusters", None).await;
    assert_eq!(status, 404);
    off.stop().await;

    let Some(dev) = start(&[("ASTER_SRV_UI_DEV_EMAIL", "dev@example.com")]).await else {
        return;
    };
    let (status, _, body) = dev.get_page("/ui", None).await;
    assert_eq!(status, 200);
    assert!(body.contains("dev@example.com"));
    dev.stop().await;
}
