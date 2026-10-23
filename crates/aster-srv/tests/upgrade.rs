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

/// A migration that meets a lock held by the previous release (a rolling
/// deploy still writing) waits it out and applies, rather than failing.
#[tokio::test]
async fn a_migration_waits_out_a_lock_held_past_its_timeout() {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return;
    };
    let database = format!("aster_srv_test_lock_{}", std::process::id());
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
    let url = format!("{base}/{database}");
    let pool = aster_srv::db::pool(&url).unwrap();
    let all = aster_srv::migrations();
    aster_srv::db::migrate(&pool, &all[..1]).await.unwrap();

    // The old release, mid-write, holds `tests` for longer than one
    // migration attempt waits.
    let (holder, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    holder
        .batch_execute("BEGIN; LOCK TABLE test_results.tests IN ROW EXCLUSIVE MODE")
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        holder.batch_execute("COMMIT").await.unwrap();
    });

    aster_srv::db::migrate(&pool, &all).await.unwrap();
    release.await.unwrap();
    let applied: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT count(*) FROM aster_srv_migrations", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(applied, all.len() as i64);

    pool.close();
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .await
        .unwrap();
}

/// The deadlock seen rolling out 0.19.0: the old release holds `runs` while
/// writing, the migration takes `tests` and waits for `runs`, and the old
/// release then needs `tests`. Postgres aborts the migration; it is tried
/// again once the old release's transaction ends.
#[tokio::test]
async fn a_migration_deadlocked_with_the_old_release_is_tried_again() {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return;
    };
    let database = format!("aster_srv_test_deadlock_{}", std::process::id());
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
    let url = format!("{base}/{database}");
    let pool = aster_srv::db::pool(&url).unwrap();
    let all = aster_srv::migrations();
    aster_srv::db::migrate(&pool, &all[..1]).await.unwrap();

    let (old, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    old.batch_execute("BEGIN; LOCK TABLE test_results.runs IN ROW EXCLUSIVE MODE")
        .await
        .unwrap();
    let old_release = tokio::spawn(async move {
        // By now the migration holds `tests` and waits for `runs`.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        old.batch_execute("LOCK TABLE test_results.tests IN ROW EXCLUSIVE MODE; COMMIT")
            .await
            .unwrap();
    });

    aster_srv::db::migrate(&pool, &all).await.unwrap();
    old_release.await.unwrap();
    let applied: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT count(*) FROM aster_srv_migrations", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(applied, all.len() as i64);

    pool.close();
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .await
        .unwrap();
}

/// Runs received before incidents existed are judged when the table is
/// made: a shard that crashed last week is an incident, so a pass on its
/// inputs after the upgrade does not make its failures flakes.
#[tokio::test]
async fn the_incidents_migration_finds_earlier_crashed_shards() {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return;
    };
    let database = format!("aster_srv_test_incidents_{}", std::process::id());
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
    assert_eq!(all[5].version, 6, "test_results migrations come first");
    aster_srv::db::migrate(&pool, &all[..5]).await.unwrap();

    // Before the upgrade: shard 6 crashed (40 of 100 failed, all with one
    // hash; one test failed only on its first attempt); shard 2 had one
    // ordinary failure; a cached replay of the crash is not a run of it.
    let client = pool.get().await.unwrap();
    let day = Utc::now().date_naive();
    aster_srv::test_results::ensure_partitions(&client, day)
        .await
        .unwrap();
    client
        .batch_execute(
            "INSERT INTO test_results.runs (run_id, repo, sha, branch, aster_version, started_at, received_at) \
                 VALUES ('crash', 'o/r', 'a', 'main', '0', now(), now() - interval '2 days'), \
                        ('replay', 'o/r', 'b', 'main', '0', now(), now() - interval '1 day'); \
             INSERT INTO test_results.target_runs (received_on, run_id, repo, address, input_hash, status, cached, duration_ms, confidence) \
                 VALUES (current_date, 'crash', 'o/r', '//c:shard-6', 'h6', 'failed', false, 1, 'complete'), \
                        (current_date, 'crash', 'o/r', '//c:shard-2', 'h2', 'failed', false, 1, 'complete'), \
                        (current_date, 'replay', 'o/r', '//c:shard-6', 'h6', 'failed', true, 1, 'complete'); \
             INSERT INTO test_results.test_results (received_on, run_id, repo, target, test_id, outcome, attempt, failure_hash) \
                 SELECT current_date, r, 'o/r', '//c:shard-6', 't' || i, \
                        CASE WHEN i <= 40 THEN 'error' ELSE 'pass' END, 1, \
                        CASE WHEN i <= 40 THEN 'already_shared' END \
                 FROM generate_series(1, 100) i, unnest(ARRAY['crash', 'replay']) r; \
             INSERT INTO test_results.test_results (received_on, run_id, repo, target, test_id, outcome, attempt, failure_hash) \
                 VALUES (current_date, 'crash', 'o/r', '//c:shard-6', 't100', 'fail', 0, 'x'); \
             INSERT INTO test_results.test_results (received_on, run_id, repo, target, test_id, outcome, attempt, failure_hash) \
                 SELECT current_date, 'crash', 'o/r', '//c:shard-2', 's' || i, \
                        CASE WHEN i = 1 THEN 'fail' ELSE 'pass' END, 1, CASE WHEN i = 1 THEN 'y' END \
                 FROM generate_series(1, 100) i;",
        )
        .await
        .unwrap();
    drop(client);

    aster_srv::db::migrate(&pool, &all).await.unwrap();
    let rows = pool
        .get()
        .await
        .unwrap()
        .query(
            "SELECT run_id, target, failures, tests, failure_hash, shared, \
                    received_at < now() - interval '1 day' AS received_then \
             FROM test_results.target_incidents",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<_, String>("run_id"), "crash");
    assert_eq!(row.get::<_, String>("target"), "//c:shard-6");
    // t100 failed on an earlier attempt and passed: it failed, but its
    // final result shares no hash.
    assert_eq!(row.get::<_, i32>("failures"), 41);
    assert_eq!(row.get::<_, i32>("tests"), 100);
    assert_eq!(
        row.get::<_, Option<String>>("failure_hash").as_deref(),
        Some("already_shared")
    );
    assert_eq!(row.get::<_, i32>("shared"), 40);
    assert!(
        row.get::<_, bool>("received_then"),
        "kept the run's received_at"
    );

    pool.close();
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .await
        .unwrap();
}
