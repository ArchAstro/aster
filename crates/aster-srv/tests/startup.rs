//! Starting while a rolling deploy holds table locks.
//!
//! Set ASTER_SRV_TEST_DATABASE_URL as in `server.rs`; without it the test skips.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aster_srv::{App, Config};

async fn status(url: &str, path: &str) -> u16 {
    aster_srv::http_client()
        .build()
        .unwrap()
        .get(format!("{url}{path}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Production incident, 2026-10-17: the previous release's classifier pass
/// held `test_results.quarantine`, release 0.21.0's migration 5 waited for
/// its lock past the liveness probe, and the pod was killed in a loop. The
/// server must answer `/healthz` while the migration waits, report not ready,
/// refuse other routes, and become ready once the lock is released.
#[tokio::test]
async fn the_server_stays_alive_while_a_migration_waits_for_a_table_lock() {
    // Setup: a database at the previous release's schema (migrations 1-4).
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return;
    };
    let database = format!("aster_srv_test_startup_{}", std::process::id());
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
    let database_url = format!("{base}/{database}");
    let setup = aster_srv::db::pool(&database_url).unwrap();
    let all = aster_srv::migrations();
    aster_srv::db::migrate(&setup, &all[..4]).await.unwrap();
    setup.close();

    // The old release's open transaction, reading a table migration 5 alters.
    let (mut old_release, connection) =
        tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
            .await
            .unwrap();
    tokio::spawn(connection);
    let old_pass = old_release.transaction().await.unwrap();
    old_pass
        .batch_execute("LOCK TABLE test_results.quarantine IN ACCESS SHARE MODE")
        .await
        .unwrap();

    // Boundary: the new release serves while its migration is blocked.
    let config = Config::from_env(|name| match name {
        "ASTER_SRV_DATABASE_URL" => Some(database_url.clone()),
        "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
        _ => None,
    })
    .unwrap();
    let app = App::connect(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(aster_srv::serve(Arc::clone(&app), listener, async {
        let _ = stopped.await;
    }));
    let migrating = tokio::spawn({
        let app = Arc::clone(&app);
        async move { app.migrate().await }
    });

    // Outcome while blocked (longer than one 5s lock timeout, so at least one
    // attempt has failed): alive, not ready, other routes refused.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(!migrating.is_finished(), "the migration should still wait");
    assert_eq!(status(&url, "/healthz").await, 200);
    assert_eq!(status(&url, "/readyz").await, 503);
    assert_eq!(status(&url, "/v1/test-results/durations").await, 503);

    // Release the lock: the migration finishes and the server becomes ready.
    old_pass.rollback().await.unwrap();
    migrating.await.unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while status(&url, "/readyz").await != 200 {
        assert!(Instant::now() < deadline, "readyz never became 200");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = shutdown.send(());
    app.pool.close();
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .await
        .unwrap();
}
