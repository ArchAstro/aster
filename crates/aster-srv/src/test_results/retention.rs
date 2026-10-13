//! Keeping the retention period: whole day partitions older than it are
//! dropped, and runs and tests not seen within it are deleted.

use std::time::Duration;

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use serde_json::json;

use super::{ensure_partitions, partition_day, PARTITIONED};
use crate::App;

const RETENTION_LOCK: i64 = 0x6173_7465_722d_7274; // "aster-rt"

/// What one pass did; `None` when another replica held the lock.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub dropped: Vec<String>,
    pub runs_deleted: u64,
    pub tests_deleted: u64,
}

/// One pass as of `today`: create the coming days' partitions, then remove
/// what is older than `retention_days`. Safe to run again; replicas take
/// turns through an advisory lock.
pub async fn run_once(app: &App, today: NaiveDate) -> Result<Option<Report>> {
    let mut client = app.pool.get().await?;
    for ahead in 0..3 {
        ensure_partitions(&client, today + chrono::Days::new(ahead)).await?;
    }
    let transaction = client.transaction().await?;
    let locked: bool = transaction
        .query_one("SELECT pg_try_advisory_xact_lock($1)", &[&RETENTION_LOCK])
        .await?
        .get(0);
    if !locked {
        return Ok(None);
    }
    let cutoff = today - chrono::Days::new(u64::from(app.config.retention_days));
    let mut report = Report::default();
    let partitions = transaction
        .query(
            "SELECT child.relname::text, parent.relname::text \
             FROM pg_inherits \
             JOIN pg_class child ON child.oid = pg_inherits.inhrelid \
             JOIN pg_class parent ON parent.oid = pg_inherits.inhparent \
             JOIN pg_namespace ns ON ns.oid = parent.relnamespace \
             WHERE ns.nspname = 'test_results' AND parent.relname = ANY($1)",
            &[&PARTITIONED.to_vec()],
        )
        .await?;
    for row in partitions {
        let name: String = row.get(0);
        if partition_day(&name).is_some_and(|day| day < cutoff) {
            transaction
                .batch_execute(&format!("DROP TABLE test_results.{name}"))
                .await?;
            report.dropped.push(name);
        }
    }
    report.dropped.sort();
    let cutoff_at = cutoff.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
    report.runs_deleted = transaction
        .execute(
            "DELETE FROM test_results.runs WHERE received_at < $1",
            &[&cutoff_at],
        )
        .await?;
    report.tests_deleted = transaction
        .execute(
            "DELETE FROM test_results.tests WHERE last_seen < $1",
            &[&cutoff_at],
        )
        .await?;
    transaction.commit().await?;
    Ok(Some(report))
}

/// Run a pass now and every six hours after.
pub fn spawn(app: std::sync::Arc<App>) {
    tokio::spawn(async move {
        loop {
            match run_once(&app, Utc::now().date_naive()).await {
                Ok(Some(report)) => crate::log::info(
                    "retention pass",
                    json!({
                        "dropped": report.dropped,
                        "runs_deleted": report.runs_deleted,
                        "tests_deleted": report.tests_deleted,
                    }),
                ),
                Ok(None) => {}
                Err(error) => crate::log::error(
                    "retention pass failed",
                    json!({"error": format!("{error:#}")}),
                ),
            }
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });
}
