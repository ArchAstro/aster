//! N8: the operator runbook for revoking a departed person's tokens. The SQL
//! is read from the README between the `revoke-sql` markers, so the test
//! runs what an operator would paste into `psql`, against real Postgres.
//!
//! Set `ASTER_SRV_TEST_DATABASE_URL`; without it the test skips.

mod common;

use std::sync::Arc;

use aster_srv::cache::{BlobStore, FixedSigner, Signer};
use common::{iap_subject, person_bearer, Server, IAP_AUDIENCE};
use serde_json::json;

const REPO: &str = "ArchAstro/firstlanding";
const README: &str = include_str!("../README.md");

/// The documented statement with psql's `:'subject'` bound as `$1`, and the
/// example subject its `\set` line gives. psql sends `:'subject'` as a quoted
/// text literal, which is what a bound text parameter is.
fn documented_revoke_sql() -> (String, String) {
    let block = README
        .split("<!-- revoke-sql:start -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- revoke-sql:end -->").next())
        .expect("the README has the revoke-sql markers");
    let body = block
        .trim()
        .strip_prefix("```sql")
        .and_then(|rest| rest.strip_suffix("```"))
        .expect("the snippet is one sql code block")
        .trim();
    let (set, statement) = body.split_once('\n').expect("a \\set line, then SQL");
    let example = set
        .strip_prefix("\\set subject '")
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("the first line sets the subject")
        .to_string();
    assert_eq!(statement.matches(":'subject'").count(), 1, "{statement}");
    assert!(!statement.contains('$'), "{statement}");
    (statement.replace(":'subject'", "$1"), example)
}

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
            ("ASTER_SRV_IAP_AUDIENCE", IAP_AUDIENCE),
            ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
        ],
        Some(store),
    )
    .await
}

#[tokio::test]
async fn the_documented_revoke_sql_logs_out_a_departed_person_and_nobody_else() {
    let (sql, example) = documented_revoke_sql();
    assert!(example.starts_with("accounts.google.com:"), "{example}");
    let Some(server) = start().await else {
        return;
    };
    let lookup = |bearer: String| {
        let server = &server;
        async move {
            server
                .post_json(
                    "/v1/cache/lookup",
                    &json!({"repo": REPO, "keys": ["k"]}),
                    Some(&bearer),
                    None,
                )
                .await
                .0
        }
    };

    // Two people signed in; Mallory holds tokens on two laptops.
    let departed = "mallory@example.com";
    let laptop = person_bearer(&server, departed);
    let desktop = person_bearer(&server, departed);
    let colleague = person_bearer(&server, "ada@example.com");
    for bearer in [&laptop, &desktop, &colleague] {
        assert_eq!(lookup(bearer.clone()).await, 200);
    }

    // The operator runs the README's SQL for Mallory's stable subject.
    let subject = iap_subject(departed);
    let client = server.client().await;
    assert_eq!(client.execute(sql.as_str(), &[&subject]).await.unwrap(), 1);

    // Mallory's tokens are refused at once; Ada's still works.
    assert_eq!(lookup(laptop.clone()).await, 401);
    assert_eq!(lookup(desktop).await, 401);
    assert_eq!(lookup(colleague.clone()).await, 200);

    // Running it again (the person was already revoked, by logout or an
    // earlier run) updates the row instead of failing.
    assert_eq!(client.execute(sql.as_str(), &[&subject]).await.unwrap(), 1);
    let rows: i64 = client
        .query_one(
            "SELECT count(*) FROM auth.revocations WHERE subject = $1",
            &[&subject],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 1);
    assert_eq!(lookup(laptop).await, 401);
    assert_eq!(lookup(colleague).await, 200);
    drop(client);
    server.stop().await;
}
