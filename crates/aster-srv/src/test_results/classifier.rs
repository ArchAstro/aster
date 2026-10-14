//! Telling flaky tests from broken ones.
//!
//! Every five minutes one replica (an advisory lock) looks at the runs
//! received since the last pass, writes the flake evidence they hold to
//! `test_flake_events`, and re-derives each affected test's `state` in
//! `tests`. All of it is set-based SQL over the new runs' rows.
//!
//! # Rules
//!
//! A test's result in a run is its *final* attempt (the highest `attempt`
//! per run and test). Skipped tests and tests of cached targets are ignored:
//! a skip says nothing, and a cached target replays old results instead of
//! running anything. Runs are ordered by `received_at`, then `run_id`.
//!
//! Definite flake events (`kind = 'definite'`), one per test and run:
//!
//! 1. `retry`: the run's final attempt passed and an earlier attempt of the
//!    same test failed or errored.
//! 2. `input_hash`: the test has a final outcome in this run and in an
//!    earlier run within the retention period, both with the same target
//!    `input_hash`, and one passed while the other failed or errored. The
//!    event is for the later run, compared with the nearest earlier run of
//!    that hash, so a test that keeps flipping keeps producing events but a
//!    steady one does not.
//!
//! Likely flake events (`kind = 'likely'`), only when the run is on the
//! default branch (`ASTER_SRV_DEFAULT_BRANCH`):
//!
//! 3. `next_main_run`: the final outcome is a pass and the test's previous
//!    default-branch run failed, whatever the input hash, unless that failure
//!    ended a broken streak (so fixing a broken test is not a flake) or the
//!    run already has a definite event for the test. Whether the diff between
//!    the two runs touched the test is not knowable here; same-hash cases are
//!    rule 2.
//!
//! Confidence: definite events use every result, `partial` targets included
//! (a flip is a flip, however the results were captured). Likely events and
//! broken streaks use only results that are not `partial`, because they judge
//! a failure and `partial` results may miss or misattribute tests.
//!
//! Broken: the test's last `ASTER_SRV_BROKEN_RUNS` (at least 3) default-branch
//! runs all failed with the same, non-null `failure_hash`. `broken_since_sha`
//! is the sha of the first run of that whole streak. The next pass that
//! leaves it healthy clears it. Broken wins over flaky.
//!
//! `flake_score` is 3 per definite event plus 1 per likely event observed in
//! the last 7 days; `state` is `broken`, else `flaky` when the score is
//! positive, else `healthy`.
//!
//! # Idempotence
//!
//! Events are keyed by (repo, test, kind, run) and inserted with
//! `ON CONFLICT DO NOTHING`; state, score and streaks are re-derived from
//! events and results, never incremented. Each pass re-reads the last
//! 15 minutes before its watermark, because a run's `received_at` is taken
//! before its upload commits.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::json;

use super::ensure_partition;
use crate::App;

const CLASSIFIER_LOCK: i64 = 0x6173_7465_722d_636c; // "aster-cl"

/// How often a pass runs.
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How far before its watermark a pass re-reads runs.
const OVERLAP: chrono::Duration = chrono::Duration::minutes(15);

/// The longest span of runs one transaction takes, so a first pass over a
/// full retention period works in pieces.
const CHUNK: chrono::Duration = chrono::Duration::hours(6);

/// Days of events that count toward the score.
const SCORE_DAYS: i64 = 7;

/// What a pass did; `None` when another replica held the lock.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Test results (final attempts) examined.
    pub results: u64,
    /// New events written.
    pub events: u64,
    /// `tests` rows whose state changed.
    pub tests_updated: u64,
}

/// One pass as of `now`: classify every run received up to `now` that an
/// earlier pass has not covered. Safe to run again.
pub async fn run_once(app: &App, now: DateTime<Utc>) -> Result<Option<Report>> {
    let mut report = Report::default();
    loop {
        let Some((chunk, done)) = pass(app, now).await? else {
            return Ok(None);
        };
        report.results += chunk.results;
        report.events += chunk.events;
        report.tests_updated += chunk.tests_updated;
        if done {
            return Ok(Some(report));
        }
    }
}

/// Run a pass now and every [`INTERVAL`] after.
pub fn spawn(app: std::sync::Arc<App>) {
    tokio::spawn(async move {
        loop {
            match run_once(&app, Utc::now()).await {
                Ok(Some(report)) => crate::log::info(
                    "classifier pass",
                    json!({
                        "results": report.results,
                        "events": report.events,
                        "tests_updated": report.tests_updated,
                    }),
                ),
                Ok(None) => {}
                Err(error) => crate::log::error(
                    "classifier pass failed",
                    json!({"error": format!("{error:#}")}),
                ),
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// A timestamp as a SQL literal.
fn at(time: DateTime<Utc>) -> String {
    format!(
        "'{}'::timestamptz",
        time.to_rfc3339_opts(SecondsFormat::Micros, true)
    )
}

/// A day as a SQL literal.
fn day(day: NaiveDate) -> String {
    format!("'{day}'::date")
}

/// Text as a SQL literal.
fn text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// What one transaction fixes: the window, the clock and the settings.
struct Window {
    /// Runs received in `(from, to]` are classified.
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    /// The oldest day whose results count as history.
    history_from: NaiveDate,
    /// The branch names that mean the default branch.
    on_default: String,
    broken_runs: u32,
}

impl Window {
    fn branch_is_default(&self, branch: &str) -> String {
        format!(
            "{branch} IN ({0}, {1})",
            text(&self.on_default),
            text(&format!("refs/heads/{}", self.on_default))
        )
    }

    /// Each run's final result of one test: `filter` picks the test and
    /// bounds the runs. `strict` leaves out `partial` targets.
    fn finals(&self, filter: &str, strict: bool) -> String {
        format!(
            "SELECT DISTINCT ON (tr.run_id) tr.run_id, r.sha, r.received_at, \
                    tr.outcome IN ('fail', 'error') AS is_fail, tr.failure_hash \
             FROM test_results.test_results tr \
             JOIN test_results.runs r ON r.run_id = tr.run_id \
             JOIN test_results.target_runs t \
                  ON t.run_id = tr.run_id AND t.address = tr.target \
                 AND t.received_on = tr.received_on \
             WHERE tr.outcome <> 'skip' AND NOT t.cached {strict} AND {filter} \
             ORDER BY tr.run_id, tr.attempt DESC",
            strict = if strict {
                "AND t.confidence IS DISTINCT FROM 'partial'"
            } else {
                ""
            },
        )
    }
}

/// One transaction: classify the next chunk of runs. Returns what it did
/// and whether it reached `now`, or `None` when the lock is held.
async fn pass(app: &App, now: DateTime<Utc>) -> Result<Option<(Report, bool)>> {
    let mut client = app.pool.get().await?;
    // Read without the lock; another replica that moves the watermark first
    // only makes this pass redo work, which is harmless.
    let watermark: DateTime<Utc> = match client
        .query_opt("SELECT watermark FROM test_results.classifier_state", &[])
        .await?
    {
        Some(row) => row.get(0),
        // The first pass starts at the oldest run kept.
        None => client
            .query_one(
                "SELECT COALESCE(min(received_at), $1) FROM test_results.runs",
                &[&now],
            )
            .await?
            .get(0),
    };
    let to = now.min(watermark + CHUNK);
    let done = to >= now;
    let window = Window {
        from: watermark - OVERLAP,
        to,
        history_from: now.date_naive() - chrono::Days::new(u64::from(app.config.retention_days)),
        on_default: app.config.default_branch.clone(),
        broken_runs: app.config.broken_runs,
    };

    // Event partitions for the days being classified. Another replica may
    // race to create one; that is fine.
    let mut current = window.from.date_naive();
    while current <= window.to.date_naive() {
        ensure_partition(&client, "test_flake_events", current).await?;
        current = current.succ_opt().expect("a day after");
    }

    let transaction = client.transaction().await?;
    let locked: bool = transaction
        .query_one("SELECT pg_try_advisory_xact_lock($1)", &[&CLASSIFIER_LOCK])
        .await?
        .get(0);
    if !locked {
        return Ok(None);
    }
    let report = classify(&transaction, &window).await?;
    transaction
        .execute(
            "INSERT INTO test_results.classifier_state (id, watermark) VALUES (1, $1) \
             ON CONFLICT (id) DO UPDATE SET \
                 watermark = GREATEST(test_results.classifier_state.watermark, EXCLUDED.watermark), \
                 updated_at = now()",
            &[&window.to],
        )
        .await?;
    transaction.commit().await?;
    Ok(Some((report, done)))
}

const EVENT_COLUMNS: &str = "observed_on, repo, test_id, kind, reason, run_id, prev_run_id, \
                             target, input_hash, observed_at";
const EVENT_KEY: &str = "(repo, test_id, kind, run_id, observed_on)";

async fn classify(
    transaction: &tokio_postgres::Transaction<'_>,
    window: &Window,
) -> Result<Report> {
    let from_day = window.from.date_naive();
    let to_day = window.to.date_naive();

    // The final result of every test in the new runs.
    let results = transaction
        .execute(
            &format!(
                "CREATE TEMP TABLE cls_win ON COMMIT DROP AS \
                 WITH attempts AS ( \
                     SELECT tr.repo, tr.run_id, tr.test_id, tr.target, tr.outcome, \
                            tr.failure_hash, tr.received_on, r.received_at, \
                            {on_default} AS on_default, t.input_hash, t.confidence, \
                            row_number() OVER w AS rn, \
                            bool_or(tr.outcome IN ('fail', 'error')) OVER (\
                                PARTITION BY tr.run_id, tr.test_id) AS any_fail \
                     FROM test_results.runs r \
                     JOIN test_results.test_results tr \
                          ON tr.run_id = r.run_id AND tr.received_on BETWEEN {from_day} AND {to_day} \
                     JOIN test_results.target_runs t \
                          ON t.run_id = tr.run_id AND t.address = tr.target \
                         AND t.received_on = tr.received_on \
                     WHERE r.received_at > {from} AND r.received_at <= {to} \
                       AND tr.outcome <> 'skip' AND NOT t.cached \
                     WINDOW w AS (PARTITION BY tr.run_id, tr.test_id ORDER BY tr.attempt DESC)) \
                 SELECT repo, run_id, test_id, target, outcome IN ('fail', 'error') AS is_fail, \
                        failure_hash, received_on, received_at, on_default, input_hash, \
                        confidence IS NOT DISTINCT FROM 'partial' AS partial, \
                        any_fail AND outcome = 'pass' AS retried \
                 FROM attempts WHERE rn = 1",
                on_default = window.branch_is_default("r.branch"),
                from = at(window.from),
                to = at(window.to),
                from_day = day(from_day),
                to_day = day(to_day),
            ),
            &[],
        )
        .await?;
    transaction.batch_execute("ANALYZE cls_win").await?;

    let history = day(window.history_from);
    let mut events = 0;

    // Rule 1: a later attempt passed after an earlier one failed.
    events += transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT w.received_on, w.repo, w.test_id, 'definite', 'retry', w.run_id, \
                        NULL, w.target, w.input_hash, w.received_at \
                 FROM cls_win w WHERE w.retried \
                 ON CONFLICT {EVENT_KEY} DO NOTHING"
            ),
            &[],
        )
        .await?;

    // Rule 2: the same input hash gave a different outcome than the nearest
    // earlier run of that hash.
    let earlier_same_hash = window.finals(
        &format!(
            "tr.repo = w.repo AND tr.test_id = w.test_id AND tr.received_on >= {history} \
             AND tr.received_on <= w.received_on AND t.input_hash = w.input_hash \
             AND (r.received_at, r.run_id) < (w.received_at, w.run_id)"
        ),
        false,
    );
    events += transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT w.received_on, w.repo, w.test_id, 'definite', 'input_hash', w.run_id, \
                        p.run_id, w.target, w.input_hash, w.received_at \
                 FROM cls_win w \
                 CROSS JOIN LATERAL ( \
                     SELECT f.run_id, f.is_fail FROM ({earlier_same_hash}) f \
                     ORDER BY f.received_at DESC, f.run_id DESC LIMIT 1) p \
                 WHERE w.input_hash IS NOT NULL AND p.is_fail <> w.is_fail \
                 ON CONFLICT {EVENT_KEY} DO NOTHING"
            ),
            &[],
        )
        .await?;

    // Rule 3: a default-branch pass right after a default-branch failure
    // that did not end a broken streak.
    let previous_main = window.finals(
        &format!(
            "tr.repo = w.repo AND tr.test_id = w.test_id AND tr.received_on >= {history} \
             AND tr.received_on <= w.received_on AND {main} \
             AND (r.received_at, r.run_id) < (w.received_at, w.run_id)",
            main = window.branch_is_default("r.branch"),
        ),
        true,
    );
    let streak_before = window.finals(
        &format!(
            "tr.repo = w.repo AND tr.test_id = w.test_id AND tr.received_on >= {history} \
             AND tr.received_on <= w.received_on AND {main} \
             AND (r.received_at, r.run_id) <= (p.received_at, p.run_id)",
            main = window.branch_is_default("r.branch"),
        ),
        true,
    );
    events += transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT w.received_on, w.repo, w.test_id, 'likely', 'next_main_run', w.run_id, \
                        p.run_id, w.target, w.input_hash, w.received_at \
                 FROM cls_win w \
                 CROSS JOIN LATERAL ( \
                     SELECT f.run_id, f.received_at, f.is_fail FROM ({previous_main}) f \
                     ORDER BY f.received_at DESC, f.run_id DESC LIMIT 1) p \
                 WHERE w.on_default AND NOT w.partial AND NOT w.is_fail AND p.is_fail \
                   AND NOT EXISTS (SELECT 1 FROM test_results.test_flake_events e \
                                   WHERE e.repo = w.repo AND e.test_id = w.test_id \
                                     AND e.run_id = w.run_id AND e.kind = 'definite' \
                                     AND e.observed_on = w.received_on) \
                   AND NOT COALESCE((SELECT count(*) = {k} \
                                            AND bool_and(s.is_fail AND s.failure_hash IS NOT NULL) \
                                            AND count(DISTINCT s.failure_hash) = 1 \
                                     FROM (SELECT f.is_fail, f.failure_hash \
                                           FROM ({streak_before}) f \
                                           ORDER BY f.received_at DESC, f.run_id DESC \
                                           LIMIT {k}) s), false) \
                 ON CONFLICT {EVENT_KEY} DO NOTHING",
                k = window.broken_runs,
            ),
            &[],
        )
        .await?;

    // State: re-derived for the tests in the new runs and for every test
    // that is not healthy, whose score ages or whose streak may end.
    let latest_main = window.finals(
        &format!(
            "tr.repo = a.repo AND tr.test_id = a.test_id AND tr.received_on >= {history} \
             AND {main} AND r.received_at <= {to}",
            main = window.branch_is_default("r.branch"),
            to = at(window.to),
        ),
        true,
    );
    let tests_updated = transaction
        .execute(
            &format!(
                "WITH affected AS ( \
                     SELECT repo, test_id FROM cls_win \
                     UNION \
                     SELECT repo, test_id FROM test_results.tests \
                     WHERE state <> 'healthy' OR flake_score > 0), \
                 scores AS ( \
                     SELECT a.repo, a.test_id, \
                            COALESCE(sum(CASE e.kind WHEN 'definite' THEN 3 ELSE 1 END) \
                                FILTER (WHERE e.observed_at > {to} - interval '{SCORE_DAYS} days'), 0)::int AS score, \
                            max(e.observed_at) AS last_flake_at \
                     FROM affected a \
                     LEFT JOIN test_results.test_flake_events e \
                          ON e.repo = a.repo AND e.test_id = a.test_id \
                         AND e.observed_on >= {history} AND e.observed_at <= {to} \
                     GROUP BY a.repo, a.test_id), \
                 mains AS ( \
                     SELECT a.repo, a.test_id, f.* FROM affected a \
                     CROSS JOIN LATERAL ({latest_main}) f), \
                 marked AS ( \
                     SELECT *, first_value(failure_hash) OVER latest AS latest_hash \
                     FROM mains \
                     WINDOW latest AS (PARTITION BY repo, test_id \
                                       ORDER BY received_at DESC, run_id DESC)), \
                 streaking AS ( \
                     SELECT *, bool_and(is_fail AND failure_hash IS NOT NULL \
                                        AND failure_hash = latest_hash) \
                                   OVER (PARTITION BY repo, test_id \
                                         ORDER BY received_at DESC, run_id DESC) AS in_streak \
                     FROM marked), \
                 streaks AS ( \
                     SELECT repo, test_id, count(*) FILTER (WHERE in_streak) AS length, \
                            (array_agg(sha ORDER BY received_at, run_id) \
                                 FILTER (WHERE in_streak))[1] AS since_sha, \
                            max(latest_hash) AS failure_hash \
                     FROM streaking GROUP BY repo, test_id), \
                 derived AS ( \
                     SELECT s.repo, s.test_id, s.score, s.last_flake_at, \
                            COALESCE(k.length, 0) >= {k} AS broken, \
                            k.since_sha, k.failure_hash \
                     FROM scores s LEFT JOIN streaks k USING (repo, test_id)) \
                 UPDATE test_results.tests t SET \
                     state = CASE WHEN d.broken THEN 'broken' \
                                  WHEN d.score > 0 THEN 'flaky' ELSE 'healthy' END, \
                     flake_score = d.score, \
                     last_flake_at = d.last_flake_at, \
                     broken_since_sha = CASE WHEN d.broken THEN d.since_sha END, \
                     broken_failure_hash = CASE WHEN d.broken THEN d.failure_hash END \
                 FROM derived d \
                 WHERE t.repo = d.repo AND t.test_id = d.test_id \
                   AND (t.state, t.flake_score, t.last_flake_at, \
                        t.broken_since_sha, t.broken_failure_hash) \
                       IS DISTINCT FROM \
                       (CASE WHEN d.broken THEN 'broken' \
                             WHEN d.score > 0 THEN 'flaky' ELSE 'healthy' END, \
                        d.score, d.last_flake_at, \
                        CASE WHEN d.broken THEN d.since_sha END, \
                        CASE WHEN d.broken THEN d.failure_hash END)",
                to = at(window.to),
                k = window.broken_runs,
            ),
            &[],
        )
        .await?;

    Ok(Report {
        results,
        events,
        tests_updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_quoted() {
        assert_eq!(text("it's"), "'it''s'");
        assert_eq!(
            at("2026-10-14T01:02:03.5Z".parse().unwrap()),
            "'2026-10-14T01:02:03.500000Z'::timestamptz"
        );
        assert_eq!(
            day(NaiveDate::from_ymd_opt(2026, 10, 14).unwrap()),
            "'2026-10-14'::date"
        );
    }
}
