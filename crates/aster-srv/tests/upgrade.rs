//! Upgrading a database that already holds data from an earlier release.

use chrono::{DateTime, Utc};

/// A database that already holds version 1's data takes version 2 in place:
/// existing tests come out healthy with a zero score.
#[tokio::test]
async fn the_second_migration_applies_over_existing_data() {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return;
    };
    let database = format!("aster_srv_test_upgrade_{}", std::process::id());
    let (admin, connection) = tokio_postgres::connect(&admin_url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .await
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .await
        .unwrap();
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let pool = aster_srv::db::pool(&format!("{base}/{database}")).unwrap();

    let all = aster_srv::migrations();
    aster_srv::db::migrate(&pool, &all[..1]).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO test_results.runs (run_id, repo, sha, branch, aster_version, started_at) \
                 VALUES ('old', 'ArchAstro/firstlanding', 'abc', 'main', '0', now()); \
             INSERT INTO test_results.tests (repo, test_id, target, name, first_seen, last_seen) \
                 VALUES ('ArchAstro/firstlanding', 't', '//a:test', 't', now(), now());",
        )
        .await
        .unwrap();
    drop(client);

    aster_srv::db::migrate(&pool, &all).await.unwrap();
    let row = pool
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT state, flake_score, last_flake_at, broken_since_sha FROM test_results.tests",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "healthy");
    assert_eq!(row.get::<_, i32>(1), 0);
    assert!(row.get::<_, Option<DateTime<Utc>>>(2).is_none());
    assert!(row.get::<_, Option<String>>(3).is_none());

    pool.close();
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .await
        .unwrap();
}
