//! CI test results: Aster uploads each invocation's results stream, and the
//! server keeps it for the retention period.
//!
//! Everything lives in the `test_results` Postgres schema. `runs` is small
//! and keyed by `run_id`, which makes an upload idempotent. `target_runs`
//! and `test_results` are large and partitioned by the UTC day they were
//! received, so retention drops whole days. `tests` holds each test's
//! identity, when it was first and last seen, and what the classifier
//! concluded about it. `test_flake_events` (also day-partitioned) is the
//! classifier's evidence, and `target_incidents` the target runs whose
//! tests failed together for one cause. `quarantine` lists the tests held
//! out of failing builds, and `quarantine_log` how that list changed.

pub mod classifier;
pub mod durations;
pub mod ingest;
pub mod quarantine;
pub mod query;
pub mod retention;

use chrono::NaiveDate;

use crate::db::Migration;

pub const MIGRATIONS: &[Migration] = &[
    Migration {
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
    },
    Migration {
        feature: "test_results",
        version: 2,
        // Additive only. The new `tests` columns have constant defaults (no
        // rewrite) and the new indexes are on small tables: `tests` (one row
        // per test) and `runs`. The big partitioned tables are untouched and
        // the events table starts with no partitions.
        sql: r#"
ALTER TABLE test_results.tests
    ADD COLUMN state text NOT NULL DEFAULT 'healthy'
        CHECK (state IN ('healthy', 'flaky', 'broken')),
    ADD COLUMN flake_score integer NOT NULL DEFAULT 0,
    ADD COLUMN last_flake_at timestamptz,
    ADD COLUMN broken_since_sha text,
    ADD COLUMN broken_failure_hash text;
CREATE INDEX tests_by_score ON test_results.tests (repo, flake_score DESC, test_id);
CREATE INDEX tests_by_state_score ON test_results.tests (repo, state, flake_score DESC, test_id);
CREATE INDEX runs_received ON test_results.runs (received_at);

CREATE TABLE test_results.classifier_state (
    id smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    watermark timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE test_results.test_flake_events (
    observed_on date NOT NULL,
    repo text NOT NULL,
    test_id text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('definite', 'likely')),
    reason text NOT NULL,
    run_id text NOT NULL,
    prev_run_id text,
    target text NOT NULL,
    input_hash text,
    observed_at timestamptz NOT NULL
) PARTITION BY RANGE (observed_on);
CREATE UNIQUE INDEX test_flake_events_key
    ON test_results.test_flake_events (repo, test_id, kind, run_id, observed_on);
CREATE INDEX test_flake_events_test
    ON test_results.test_flake_events (repo, test_id, observed_at);

CREATE VIEW test_results.failure_clusters AS
SELECT repo,
       failure_hash,
       count(*) AS failures,
       count(DISTINCT test_id) AS tests,
       min(received_on) AS first_seen,
       max(received_on) AS last_seen,
       (array_agg(message ORDER BY received_on DESC))[1] AS example_message,
       (array_agg(test_id ORDER BY received_on DESC))[1] AS example_test_id
FROM test_results.test_results
WHERE outcome IN ('fail', 'error') AND failure_hash IS NOT NULL
GROUP BY repo, failure_hash;
"#,
    },
    Migration {
        feature: "test_results",
        version: 3,
        // The dashboard's failure clusters read one repository's failures
        // from the last days. Without this, that scans every result of those
        // days (about 0.4M a day, 4x slower on a seeded 12M-row copy); with
        // it, only the failing rows. The index is partial, so it stays a
        // few percent of the table. Creating it on the partitioned parent
        // takes a SHARE lock on each partition until the migration commits,
        // which holds back uploads for about the build time (about a second
        // for 30 days of data) and waits behind or is retried after
        // in-flight ones, within the migration lock timeout.
        sql: r#"
CREATE INDEX test_results_failures ON test_results.test_results (repo, received_on)
    WHERE outcome IN ('fail', 'error') AND failure_hash IS NOT NULL;
"#,
    },
    Migration {
        feature: "test_results",
        version: 4,
        // New tables only.
        sql: quarantine::MIGRATION_SQL,
    },
    Migration {
        feature: "test_results",
        version: 5,
        // Each test's latest default-branch result and failure streak, and
        // each quarantined test's passes, so a classifier pass reads one row
        // per test instead of its history. A new table and new columns with
        // constant defaults: the next pass fills them.
        sql: r#"
CREATE TABLE test_results.test_main (
    repo text NOT NULL,
    test_id text NOT NULL,
    run_id text NOT NULL,
    received_at timestamptz NOT NULL,
    received_on date NOT NULL,
    sha text NOT NULL,
    is_fail boolean NOT NULL,
    failure_hash text,
    streak integer NOT NULL,
    streak_since_sha text,
    PRIMARY KEY (repo, test_id)
);
ALTER TABLE test_results.classifier_state ADD COLUMN main_through timestamptz;
-- A quarantined test's passes in a row since it entered, counted up to
-- `passes_through`; NULL makes the next pass count them from the results.
ALTER TABLE test_results.quarantine
    ADD COLUMN passes integer NOT NULL DEFAULT 0,
    ADD COLUMN passes_through timestamptz;
"#,
    },
    Migration {
        feature: "test_results",
        version: 6,
        // A new table, filled from the retained results with the classifier's
        // rules (`classifier::INCIDENT_*`), so a later pass on an old crashed
        // run's inputs finds it is an incident. Only target runs with five or
        // more failing tests are read in full, a small share of the results.
        // Flake events written before this belong to the later run of a pair;
        // they age out of the score in seven days.
        sql: r#"
CREATE TABLE test_results.target_incidents (
    repo text NOT NULL,
    run_id text NOT NULL,
    target text NOT NULL,
    received_on date NOT NULL,
    received_at timestamptz NOT NULL,
    input_hash text,
    failures integer NOT NULL,
    tests integer NOT NULL,
    failure_hash text,
    shared integer NOT NULL,
    PRIMARY KEY (run_id, target)
);
CREATE INDEX target_incidents_repo ON test_results.target_incidents (repo, received_at);

WITH candidates AS (
    SELECT run_id, target, received_on FROM test_results.test_results
    WHERE outcome IN ('fail', 'error')
    GROUP BY run_id, target, received_on
    HAVING count(DISTINCT test_id) >= 5),
per_test AS (
    SELECT tr.repo, tr.run_id, tr.target, tr.test_id, tr.received_on,
           bool_or(tr.outcome IN ('fail', 'error')) AS any_fail,
           (array_agg(tr.outcome ORDER BY tr.attempt DESC))[1] IN ('fail', 'error') AS is_fail,
           (array_agg(tr.failure_hash ORDER BY tr.attempt DESC))[1] AS failure_hash
    FROM candidates c
    JOIN test_results.test_results tr
         ON tr.run_id = c.run_id AND tr.target = c.target AND tr.received_on = c.received_on
    JOIN test_results.target_runs t
         ON t.run_id = tr.run_id AND t.address = tr.target AND t.received_on = tr.received_on
    WHERE tr.outcome <> 'skip' AND NOT t.cached
    GROUP BY tr.repo, tr.run_id, tr.target, tr.test_id, tr.received_on),
runs AS (
    SELECT repo, run_id, target, received_on,
           count(*) FILTER (WHERE any_fail)::int AS failures, count(*)::int AS tests
    FROM per_test GROUP BY repo, run_id, target, received_on),
hashes AS (
    SELECT DISTINCT ON (run_id, target) run_id, target, failure_hash, count(*)::int AS shared
    FROM per_test WHERE is_fail AND failure_hash IS NOT NULL
    GROUP BY run_id, target, failure_hash
    ORDER BY run_id, target, count(*) DESC, failure_hash)
INSERT INTO test_results.target_incidents
    (repo, run_id, target, received_on, received_at, input_hash,
     failures, tests, failure_hash, shared)
SELECT c.repo, c.run_id, c.target, c.received_on, r.received_at, t.input_hash,
       c.failures, c.tests, h.failure_hash, COALESCE(h.shared, 0)
FROM runs c
JOIN test_results.runs r ON r.run_id = c.run_id
JOIN test_results.target_runs t
     ON t.run_id = c.run_id AND t.address = c.target AND t.received_on = c.received_on
LEFT JOIN hashes h ON h.run_id = c.run_id AND h.target = c.target
WHERE c.failures >= 20
   OR (c.failures >= 5 AND c.failures * 2 >= c.tests)
   OR COALESCE(h.shared, 0) >= 10
ON CONFLICT (run_id, target) DO NOTHING;
"#,
    },
    Migration {
        feature: "test_results",
        version: 7,
        // Where each run came from, from the token that uploaded it rather
        // than the run record: `source` (`oidc` or `static`) and the OIDC
        // `ref` and `event_name` claims. Nullable columns on a small table;
        // runs stored before have none and are never trusted. Each flake
        // event says whether both of its runs are trusted, which
        // quarantine requires; a constant default, so no rewrite.
        sql: r#"
ALTER TABLE test_results.runs
    ADD COLUMN source text,
    ADD COLUMN git_ref text,
    ADD COLUMN event_name text;
ALTER TABLE test_results.test_flake_events
    ADD COLUMN trusted boolean NOT NULL DEFAULT false;
"#,
    },
];

/// The day-partitioned tables.
pub const PARTITIONED: [&str; 3] = ["target_runs", "test_results", "test_flake_events"];

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
    for table in PARTITIONED {
        ensure_partition(client, table, day).await?;
    }
    Ok(())
}

/// [`ensure_partitions`] for one table.
pub async fn ensure_partition(
    client: &deadpool_postgres::Client,
    table: &str,
    day: NaiveDate,
) -> anyhow::Result<()> {
    let next = day.succ_opt().expect("a day after");
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_incident_backfill_uses_the_classifiers_thresholds() {
        use super::classifier::{INCIDENT_FAILURES, INCIDENT_SHARED_HASH, INCIDENT_SHARE_MIN};
        let backfill = MIGRATIONS
            .iter()
            .find(|m| m.version == 6)
            .expect("migration 6")
            .sql;
        for rule in [
            format!("c.failures >= {INCIDENT_FAILURES}\n"),
            format!("(c.failures >= {INCIDENT_SHARE_MIN} AND c.failures * 2 >= c.tests)"),
            format!("COALESCE(h.shared, 0) >= {INCIDENT_SHARED_HASH}\n"),
            format!("HAVING count(DISTINCT test_id) >= {INCIDENT_SHARE_MIN})"),
        ] {
            assert!(backfill.contains(&rule), "{rule}");
        }
    }

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
