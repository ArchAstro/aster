//! Postgres: the connection pool and schema migrations.
//!
//! Each feature owns its migrations, numbered from 1 and applied in order
//! once. Replicas starting together take a session advisory lock so only
//! one applies them.

use anyhow::{Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};

/// One schema change.
pub struct Migration {
    /// The feature it belongs to, e.g. `test_results`.
    pub feature: &'static str,
    pub version: i32,
    pub sql: &'static str,
}

/// A pool for `url`. Connections are plain TCP: in production the Cloud SQL
/// Auth Proxy beside the server encrypts them.
pub fn pool(url: &str) -> Result<Pool> {
    let config: tokio_postgres::Config = url.parse().context("cannot read the database URL")?;
    let manager = Manager::from_config(
        config,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(16)
        .build()
        .context("cannot build the database pool")
}

const MIGRATION_LOCK: i64 = 0x6173_7465_722d_7372; // "aster-sr"

/// Apply every migration not yet applied.
pub async fn migrate(pool: &Pool, migrations: &[&Migration]) -> Result<()> {
    let mut client = pool.get().await.context("cannot connect to the database")?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS aster_srv_migrations (
                feature text NOT NULL,
                version integer NOT NULL,
                applied_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (feature, version)
            )",
        )
        .await?;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK])
        .await?;
    let result = async {
        for migration in migrations {
            let applied = client
                .query_opt(
                    "SELECT 1 FROM aster_srv_migrations WHERE feature = $1 AND version = $2",
                    &[&migration.feature, &migration.version],
                )
                .await?
                .is_some();
            if applied {
                continue;
            }
            let transaction = client.transaction().await?;
            transaction
                .batch_execute(migration.sql)
                .await
                .with_context(|| {
                    format!(
                        "migration {} {} failed",
                        migration.feature, migration.version
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO aster_srv_migrations (feature, version) VALUES ($1, $2)",
                    &[&migration.feature, &migration.version],
                )
                .await?;
            transaction.commit().await?;
            crate::log::info(
                "applied migration",
                serde_json::json!({"feature": migration.feature, "version": migration.version}),
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK])
        .await?;
    result
}
