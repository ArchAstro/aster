//! CI test results: Aster uploads each invocation's results stream, and the
//! server keeps it for the retention period.
//!
//! Everything lives in the `test_results` Postgres schema. `runs` is small
//! and keyed by `run_id`, which makes an upload idempotent. `target_runs`
//! and `test_results` are large and partitioned by the UTC day they were
//! received, so retention drops whole days. `tests` holds each test's
//! identity and when it was first and last seen.

pub mod ingest;
pub mod retention;

use chrono::NaiveDate;

use crate::db::Migration;

pub const MIGRATIONS: &[Migration] = &[Migration {
    feature: "test_results",
    version: 1,
    sql: r#"
CREATE SCHEMA IF NOT EXISTS test_results;

CREATE TABLE test_results.runs (
    run_id text PRIMARY KEY,
    repo text NOT NULL,
    sha text NOT NULL,
    branch text,
    pr bigint,
    workflow text,
    job text,
    attempt integer,
    lane text,
    runner text,
    aster_version text NOT NULL,
    started_at timestamptz NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX runs_repo_received ON test_results.runs (repo, received_at);

CREATE TABLE test_results.target_runs (
    received_on date NOT NULL,
    run_id text NOT NULL,
    repo text NOT NULL,
    address text NOT NULL,
    input_hash text,
    status text NOT NULL,
    cached boolean NOT NULL,
    duration_ms bigint NOT NULL,
    capture text,
    confidence text,
    exit_code integer
) PARTITION BY RANGE (received_on);
CREATE INDEX target_runs_run ON test_results.target_runs (run_id);
CREATE INDEX target_runs_hash ON test_results.target_runs (repo, address, input_hash);

CREATE TABLE test_results.test_results (
    received_on date NOT NULL,
    run_id text NOT NULL,
    repo text NOT NULL,
    target text NOT NULL,
    test_id text NOT NULL,
    outcome text NOT NULL,
    attempt integer NOT NULL,
    duration_ms bigint,
    failure_hash text,
    message text
) PARTITION BY RANGE (received_on);
CREATE INDEX test_results_test ON test_results.test_results (repo, test_id, received_on);
CREATE INDEX test_results_run ON test_results.test_results (run_id);

CREATE TABLE test_results.tests (
    repo text NOT NULL,
    test_id text NOT NULL,
    target text NOT NULL,
    file text,
    suite text,
    name text NOT NULL,
    first_seen timestamptz NOT NULL,
    last_seen timestamptz NOT NULL,
    PRIMARY KEY (repo, test_id)
);
CREATE INDEX tests_last_seen ON test_results.tests (last_seen);
"#,
}];

/// The day-partitioned tables.
pub const PARTITIONED: [&str; 2] = ["target_runs", "test_results"];

/// The partition of `table` holding `day`, e.g. `test_results_p20261013`.
pub fn partition_name(table: &str, day: NaiveDate) -> String {
    format!("{table}_p{}", day.format("%Y%m%d"))
}

/// The day a partition holds, from its name.
pub fn partition_day(name: &str) -> Option<NaiveDate> {
    let (_, date) = name.rsplit_once("_p")?;
    NaiveDate::parse_from_str(date, "%Y%m%d").ok()
}

/// Create the partitions for `day` if they do not exist. Two replicas may
/// race to create one; losing that race is fine.
pub async fn ensure_partitions(
    client: &deadpool_postgres::Client,
    day: NaiveDate,
) -> anyhow::Result<()> {
    let next = day.succ_opt().expect("a day after");
    for table in PARTITIONED {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS test_results.{} PARTITION OF test_results.{table} \
             FOR VALUES FROM ('{day}') TO ('{next}')",
            partition_name(table, day),
        );
        if let Err(error) = client.batch_execute(&sql).await {
            let raced = error
                .code()
                .is_some_and(|code| *code == tokio_postgres::error::SqlState::DUPLICATE_TABLE);
            if !raced {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_names_round_trip() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        let name = partition_name("test_results", day);
        assert_eq!(name, "test_results_p20261013");
        assert_eq!(partition_day(&name), Some(day));
        assert_eq!(
            partition_day("target_runs_p20260101"),
            NaiveDate::from_ymd_opt(2026, 1, 1)
        );
        assert_eq!(partition_day("tests"), None);
    }
}
