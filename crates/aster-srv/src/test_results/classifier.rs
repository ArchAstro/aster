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
//! An event belongs to the run that *failed* (`run_id`; `prev_run_id` is
//! the passing run it was compared with) and is observed when that run was
//! received. One failure is one event however many passes it is compared
//! with: a pass, a failure and a pass with one input hash is a single
//! flake, not two.
//!
//! Definite flake events (`kind = 'definite'`), one per test and failing
//! run:
//!
//! 1. `retry`: the run's final attempt passed and an earlier attempt of the
//!    same test failed or errored. The run is the failing one.
//! 2. `input_hash`: the test has a final outcome in this run and in an
//!    earlier run within the retention period, both with the same target
//!    `input_hash`, and one passed while the other failed or errored. Each
//!    run is compared with the nearest earlier run of that hash, so a test
//!    that keeps flipping keeps producing events but a steady one does not.
//!
//! Likely flake events (`kind = 'likely'`), only when the run is on the
//! default branch (`ASTER_SRV_DEFAULT_BRANCH`):
//!
//! 3. `next_main_run`: the final outcome is a pass and the test's previous
//!    default-branch run failed, whatever the input hash, unless that failure
//!    ended a broken streak (so fixing a broken test is not a flake). The
//!    event is for the failed run; a definite event for that run replaces
//!    it, whichever is found first. Whether the diff between the two runs
//!    touched the test is not knowable here; same-hash cases are rule 2.
//!
//! Incidents: a target run whose tests fail together failed for one shared
//! cause (a crashed shard, a dead database, a harness process that died),
//! not because each test is flaky. A non-cached target run is an incident
//! when its failing tests (any attempt failed) number at least
//! [`INCIDENT_FAILURES`], or at least [`INCIDENT_SHARE_MIN`] and half its
//! tests, or when [`INCIDENT_SHARED_HASH`] of its final failures share one
//! failure hash. It is one row in `target_incidents`, and its failures make
//! no flake events of either kind. They still count toward broken streaks:
//! a change that breaks many tests at once is still broken.
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
//! Then the pass updates quarantine (see [`super::quarantine`]) and records
//! webhook notifications for what changed: a test entering, leaving or
//! expiring from quarantine, and a test becoming broken.
//!
//! # Idempotence
//!
//! Events are keyed by (repo, test, kind, run) and inserted with
//! `ON CONFLICT DO NOTHING`; state, score and streaks are re-derived from
//! events and results, never incremented. Each pass re-reads the last
//! 15 minutes before its watermark, because a run's `received_at` is taken
//! before its upload commits.
//!
//! # Cost
//!
//! A pass reads the new runs' results, not each test's history. Rule 2
//! starts from the target runs with the same input hash, which are few.
//! Rule 3 and broken streaks read `test_main`: each test's last
//! default-branch result and failure streak up to the window's start
//! (`classifier_state.main_through`), followed by the window's own results;
//! a pass then moves results older than its overlap into it. Quarantine
//! release counts are kept the same way (`quarantine.passes`). A window that
//! starts anywhere else rebuilds both from the results. Rule 2 compares runs
//! of the same target address, and a failure streak longer than the
//! retention period keeps counting past it.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::json;

use super::{ensure_partition, quarantine};
use crate::config::QuarantineRules;
use crate::webhooks::{self, Notification};
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

/// A target run with this many failing tests is an incident.
pub const INCIDENT_FAILURES: i64 = 20;

/// A target run with this many failing tests, at least half of those it
/// ran, is an incident.
pub const INCIDENT_SHARE_MIN: i64 = 5;

/// A target run with this many final failures sharing one failure hash is
/// an incident.
pub const INCIDENT_SHARED_HASH: i64 = 10;

/// What a pass did; `None` when another replica held the lock.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Test results (final attempts) examined.
    pub results: u64,
    /// New events written.
    pub events: u64,
    /// New incidents recorded.
    pub incidents: u64,
    /// `tests` rows whose state changed.
    pub tests_updated: u64,
    /// Quarantine entries and exits.
    pub quarantine: quarantine::Changes,
    /// Webhook deliveries recorded.
    pub deliveries: u64,
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
        report.incidents += chunk.incidents;
        report.tests_updated += chunk.tests_updated;
        report.quarantine.quarantined += chunk.quarantine.quarantined;
        report.quarantine.released += chunk.quarantine.released;
        report.quarantine.expired += chunk.quarantine.expired;
        report.quarantine.broken += chunk.quarantine.broken;
        report.deliveries += chunk.deliveries;
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
                        "incidents": report.incidents,
                        "tests_updated": report.tests_updated,
                        "quarantined": report.quarantine.quarantined,
                        "released": report.quarantine.released,
                        "expired": report.quarantine.expired,
                        "broken_in_quarantine": report.quarantine.broken,
                        "deliveries": report.deliveries,
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
pub(super) struct Window {
    /// Runs received in `(from, to]` are classified; `to` is the pass's
    /// clock.
    from: DateTime<Utc>,
    pub(super) to: DateTime<Utc>,
    /// The oldest day whose results count as history.
    history_from: NaiveDate,
    /// The branch names that mean the default branch.
    on_default: String,
    broken_runs: u32,
    quarantine: QuarantineRules,
    webhooks: Vec<webhooks::Endpoint>,
}

impl Window {
    /// `to` as a SQL literal.
    pub(super) fn to_literal(&self) -> String {
        at(self.to)
    }

    /// `from` as a SQL literal: where state carried from earlier passes
    /// (`test_main`, quarantine pass counts) stops.
    pub(super) fn start_literal(&self) -> String {
        at(self.from)
    }

    /// Where this pass leaves carried state: results up to here are final,
    /// later ones are read again by the next pass.
    pub(super) fn through(&self) -> DateTime<Utc> {
        self.to - OVERLAP
    }

    /// [`Window::through`] as a SQL literal.
    pub(super) fn through_literal(&self) -> String {
        at(self.through())
    }

    fn branch_is_default(&self, branch: &str) -> String {
        format!(
            "{branch} IN ({0}, {1})",
            text(&self.on_default),
            text(&format!("refs/heads/{}", self.on_default))
        )
    }

    /// Each run's final result of one test: `filter` picks the test and
    /// bounds the runs. `strict` leaves out `partial` targets.
    pub(super) fn finals(&self, filter: &str, strict: bool) -> String {
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
        quarantine: app.config.quarantine,
        webhooks: app.config.webhooks.clone(),
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
    // Under the lock, a pass whose clock is behind what another replica
    // committed would re-derive state, quarantine and notifications as of an
    // earlier time: a broken test could turn healthy and be announced as
    // breaking again. That replica covered this window: skip it, and go on
    // from its watermark if `now` is later still.
    let state = transaction
        .query_opt(
            "SELECT watermark, main_through FROM test_results.classifier_state",
            &[],
        )
        .await?;
    let committed: Option<DateTime<Utc>> = state.as_ref().map(|row| row.get(0));
    let main_through: Option<DateTime<Utc>> = state.as_ref().and_then(|row| row.get(1));
    if let Some(committed) = committed.filter(|committed| *committed > window.to) {
        transaction.rollback().await?;
        return Ok(Some((Report::default(), committed >= now)));
    }
    // Another replica moved the watermark between the read above and the
    // lock: this window no longer starts where `test_main` ends. Read again.
    if committed.is_some_and(|committed| committed != watermark) {
        transaction.rollback().await?;
        return Ok(Some((Report::default(), false)));
    }
    let report = classify(&transaction, &window, main_through).await?;
    transaction
        .execute(
            "INSERT INTO test_results.classifier_state (id, watermark, main_through) \
             VALUES (1, $1, $2) \
             ON CONFLICT (id) DO UPDATE SET \
                 watermark = GREATEST(test_results.classifier_state.watermark, EXCLUDED.watermark), \
                 main_through = EXCLUDED.main_through, \
                 updated_at = now()",
            &[&window.to, &window.through()],
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
    main_through: Option<DateTime<Utc>>,
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
                            tr.failure_hash, tr.received_on, r.received_at, r.sha, \
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
                        failure_hash, received_on, received_at, sha, on_default, input_hash, \
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

    // Incidents: the window's target runs whose tests failed together. A run
    // is read whole in one window (its results arrive in one upload), so an
    // incident is decided once; a later pass re-reading it finds the same.
    let incidents = transaction
        .execute(
            &format!(
                "WITH runs AS ( \
                     SELECT repo, run_id, target, min(received_on) AS received_on, \
                            min(received_at) AS received_at, min(input_hash) AS input_hash, \
                            count(*) FILTER (WHERE is_fail OR retried)::int AS failures, \
                            count(*)::int AS tests \
                     FROM cls_win GROUP BY repo, run_id, target), \
                 hashes AS ( \
                     SELECT DISTINCT ON (run_id, target) run_id, target, failure_hash, \
                            count(*)::int AS shared \
                     FROM cls_win WHERE is_fail AND failure_hash IS NOT NULL \
                     GROUP BY run_id, target, failure_hash \
                     ORDER BY run_id, target, count(*) DESC, failure_hash) \
                 INSERT INTO test_results.target_incidents \
                     (repo, run_id, target, received_on, received_at, input_hash, \
                      failures, tests, failure_hash, shared) \
                 SELECT c.repo, c.run_id, c.target, c.received_on, c.received_at, c.input_hash, \
                        c.failures, c.tests, h.failure_hash, COALESCE(h.shared, 0) \
                 FROM runs c LEFT JOIN hashes h USING (run_id, target) \
                 WHERE c.failures >= {INCIDENT_FAILURES} \
                    OR (c.failures >= {INCIDENT_SHARE_MIN} AND c.failures * 2 >= c.tests) \
                    OR COALESCE(h.shared, 0) >= {INCIDENT_SHARED_HASH} \
                 ON CONFLICT (run_id, target) DO NOTHING"
            ),
            &[],
        )
        .await?;
    // Whether the failure of `test` in run `run` (received on `day`) was
    // part of an incident.
    let in_incident = |run: &str, test: &str, day: &str| {
        format!(
            "EXISTS (SELECT 1 FROM test_results.test_results ir \
                     JOIN test_results.target_incidents i \
                          ON i.run_id = ir.run_id AND i.target = ir.target \
                     WHERE ir.run_id = {run} AND ir.test_id = {test} AND ir.received_on = {day})"
        )
    };

    // Rule 1: a later attempt passed after an earlier one failed.
    events += transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT w.received_on, w.repo, w.test_id, 'definite', 'retry', w.run_id, \
                        NULL, w.target, w.input_hash, w.received_at \
                 FROM cls_win w WHERE w.retried \
                   AND NOT EXISTS (SELECT 1 FROM test_results.target_incidents i \
                                   WHERE i.run_id = w.run_id AND i.target = w.target) \
                 ON CONFLICT {EVENT_KEY} DO NOTHING"
            ),
            &[],
        )
        .await?;

    // `test_main` holds each test's main sequence up to `window.from`. A
    // window that starts elsewhere (the first pass, or one after another
    // replica's) rebuilds it from the results.
    if main_through != Some(window.from) {
        rebuild_main(transaction, window).await?;
    }

    // Rule 2: the same input hash gave a different outcome than the nearest
    // earlier run of that hash. Runs of one target with one hash are few
    // (`target_runs_hash`), so this starts from them, the window's own
    // included, not from each test's history. A table, not a CTE, so the
    // planner knows how few.
    transaction
        .execute(
            &format!(
                "CREATE TEMP TABLE cls_hash ON COMMIT DROP AS \
                 SELECT t.repo, t.address, t.input_hash, t.run_id, t.received_on, r.received_at \
                 FROM (SELECT repo, target, input_hash, max(received_on) AS until \
                       FROM cls_win WHERE input_hash IS NOT NULL \
                       GROUP BY repo, target, input_hash) k \
                 JOIN test_results.target_runs t \
                      ON t.repo = k.repo AND t.address = k.target \
                     AND t.input_hash = k.input_hash \
                     AND t.received_on BETWEEN {history} AND k.until \
                 JOIN test_results.runs r ON r.run_id = t.run_id \
                 WHERE NOT t.cached"
            ),
            &[],
        )
        .await?;
    transaction.batch_execute("ANALYZE cls_hash").await?;
    // The event is the failed run's, whichever of the two runs that was, so
    // a pass, a failure and a pass with one hash make one event: the second
    // comparison finds the first one's key.
    events += transaction
        .execute(
            &format!(
                "WITH earlier AS MATERIALIZED ( \
                     SELECT DISTINCT ON (tr.run_id, tr.test_id) \
                            tr.repo, tr.test_id, h.address, h.input_hash, tr.run_id, \
                            h.received_at, h.received_on, \
                            tr.outcome IN ('fail', 'error') AS is_fail \
                     FROM cls_hash h \
                     JOIN test_results.test_results tr \
                          ON tr.run_id = h.run_id AND tr.received_on = h.received_on \
                         AND tr.target = h.address \
                     WHERE tr.outcome <> 'skip' \
                     ORDER BY tr.run_id, tr.test_id, tr.attempt DESC), \
                 nearest AS ( \
                     SELECT DISTINCT ON (w.run_id, w.test_id) w.*, \
                            p.run_id AS prev_run_id, p.is_fail AS prev_is_fail, \
                            p.received_at AS prev_received_at, p.received_on AS prev_received_on \
                     FROM cls_win w \
                     JOIN earlier p \
                          ON p.repo = w.repo AND p.test_id = w.test_id \
                         AND p.address = w.target AND p.input_hash = w.input_hash \
                         AND (p.received_at, p.run_id) < (w.received_at, w.run_id) \
                     ORDER BY w.run_id, w.test_id, p.received_at DESC, p.run_id DESC), \
                 flips AS ( \
                     SELECT repo, test_id, target, input_hash, \
                            CASE WHEN is_fail THEN run_id ELSE prev_run_id END AS failed_run, \
                            CASE WHEN is_fail THEN prev_run_id ELSE run_id END AS passed_run, \
                            CASE WHEN is_fail THEN received_on ELSE prev_received_on END AS failed_on, \
                            CASE WHEN is_fail THEN received_at ELSE prev_received_at END AS failed_at \
                     FROM nearest WHERE prev_is_fail <> is_fail) \
                 INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT f.failed_on, f.repo, f.test_id, 'definite', 'input_hash', f.failed_run, \
                        f.passed_run, f.target, f.input_hash, f.failed_at \
                 FROM flips f \
                 WHERE NOT EXISTS (SELECT 1 FROM test_results.target_incidents i \
                                   WHERE i.run_id = f.failed_run AND i.target = f.target) \
                 ON CONFLICT {EVENT_KEY} DO NOTHING"
            ),
            &[],
        )
        .await?;
    // A definite event replaces a likely one for the same failed run, which
    // an earlier pass may have written before this hash's other run came.
    transaction
        .execute(
            "DELETE FROM test_results.test_flake_events l \
             USING test_results.test_flake_events d \
             WHERE l.kind = 'likely' AND d.kind = 'definite' \
               AND d.repo = l.repo AND d.test_id = l.test_id AND d.run_id = l.run_id \
               AND d.observed_on = l.observed_on \
               AND (l.repo, l.test_id) IN (SELECT repo, test_id FROM cls_win)",
            &[],
        )
        .await?;

    // The main sequence of every test whose state is re-derived: its
    // `test_main` row, then its main results in the window, each with the
    // result before it and the failure streak it ends.
    transaction
        .execute(
            &format!(
                "CREATE TEMP TABLE cls_main ON COMMIT DROP AS {}",
                streaks(&format!(
                    "SELECT m.repo, m.test_id, m.run_id, m.received_at, m.received_on, m.sha, \
                            m.is_fail, m.failure_hash, true AS is_seed, \
                            m.streak AS seed_streak, m.streak_since_sha AS seed_since \
                     FROM test_results.test_main m \
                     WHERE m.received_on >= {history} \
                       AND (EXISTS (SELECT 1 FROM cls_win w \
                                    WHERE w.repo = m.repo AND w.test_id = m.test_id) \
                            OR EXISTS (SELECT 1 FROM test_results.tests t \
                                       WHERE t.repo = m.repo AND t.test_id = m.test_id \
                                         AND (t.state <> 'healthy' OR t.flake_score > 0))) \
                     UNION ALL \
                     SELECT repo, test_id, run_id, received_at, received_on, sha, is_fail, \
                            failure_hash, false, 0, NULL::text \
                     FROM cls_win WHERE on_default AND NOT partial"
                ))
            ),
            &[],
        )
        .await?;
    transaction.batch_execute("ANALYZE cls_main").await?;

    // Rule 3: a default-branch pass right after a default-branch failure
    // that did not end a broken streak. The event is the failed run's,
    // unless that run already has a definite one.
    events += transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT m.prev_received_on, w.repo, w.test_id, 'likely', 'next_main_run', \
                        m.prev_run_id, w.run_id, f.target, t.input_hash, m.prev_received_at \
                 FROM cls_win w \
                 JOIN cls_main m \
                      ON m.repo = w.repo AND m.test_id = w.test_id AND m.run_id = w.run_id \
                     AND NOT m.is_seed \
                 JOIN LATERAL ( \
                     SELECT fr.target FROM test_results.test_results fr \
                     WHERE fr.run_id = m.prev_run_id AND fr.test_id = w.test_id \
                       AND fr.received_on = m.prev_received_on \
                     ORDER BY fr.attempt DESC LIMIT 1) f ON true \
                 LEFT JOIN test_results.target_runs t \
                      ON t.run_id = m.prev_run_id AND t.address = f.target \
                     AND t.received_on = m.prev_received_on \
                 WHERE w.on_default AND NOT w.partial AND NOT w.is_fail \
                   AND m.prev_is_fail AND m.prev_streak < {k} \
                   AND NOT EXISTS (SELECT 1 FROM test_results.test_flake_events e \
                                   WHERE e.repo = w.repo AND e.test_id = w.test_id \
                                     AND e.run_id = m.prev_run_id AND e.kind = 'definite' \
                                     AND e.observed_on = m.prev_received_on) \
                   AND NOT {incident} \
                 ON CONFLICT {EVENT_KEY} DO NOTHING",
                k = window.broken_runs,
                incident = in_incident("m.prev_run_id", "w.test_id", "m.prev_received_on"),
            ),
            &[],
        )
        .await?;

    // State: re-derived for the tests in the new runs and for every test
    // that is not healthy, whose score ages or whose streak may end.
    let tests_updated = transaction
        .query(
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
                 streaks AS ( \
                     SELECT repo, test_id, streak AS length, since AS since_sha, failure_hash \
                     FROM cls_main WHERE is_last), \
                 derived AS ( \
                     SELECT s.repo, s.test_id, s.score, s.last_flake_at, \
                            COALESCE(k.length, 0) >= {k} AS broken, \
                            k.since_sha, k.failure_hash, o.state AS old_state \
                     FROM scores s LEFT JOIN streaks k USING (repo, test_id) \
                     JOIN test_results.tests o USING (repo, test_id)) \
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
                        CASE WHEN d.broken THEN d.failure_hash END) \
                 RETURNING t.repo, t.test_id, t.target, t.file, t.suite, t.name, t.state, \
                           d.old_state, t.broken_since_sha, t.broken_failure_hash",
                to = at(window.to),
                k = window.broken_runs,
            ),
            &[],
        )
        .await?;
    let mut notifications: Vec<Notification> = tests_updated
        .iter()
        .filter(|row| {
            row.get::<_, String>("state") == "broken"
                && row.get::<_, String>("old_state") != "broken"
        })
        .map(|row| {
            quarantine::notification(
                "test.broken",
                &row.get::<_, String>("repo"),
                window.to,
                quarantine::test_json(row),
                json!({
                    "since_sha": row.get::<_, Option<String>>("broken_since_sha"),
                    "failure_hash": row.get::<_, Option<String>>("broken_failure_hash"),
                }),
            )
        })
        .collect();

    // Results old enough that no later upload can come before them move
    // into `test_main`; the rest are read again by the next pass.
    transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_main ({MAIN_COLUMNS}) \
                 SELECT DISTINCT ON (repo, test_id) repo, test_id, run_id, received_at, \
                        received_on, sha, is_fail, failure_hash, streak, since \
                 FROM cls_main WHERE NOT is_seed AND received_at <= {through} \
                 ORDER BY repo, test_id, received_at DESC, run_id DESC \
                 ON CONFLICT (repo, test_id) DO UPDATE SET {MAIN_UPDATE}",
                through = at(window.through()),
            ),
            &[],
        )
        .await?;

    let (quarantine, mut changed) =
        quarantine::update(transaction, window, window.quarantine).await?;
    notifications.append(&mut changed);
    let deliveries =
        webhooks::enqueue(transaction, &window.webhooks, &notifications, window.to).await?;

    Ok(Report {
        results,
        events,
        incidents,
        tests_updated: tests_updated.len() as u64,
        quarantine,
        deliveries,
    })
}

const MAIN_COLUMNS: &str = "repo, test_id, run_id, received_at, received_on, sha, is_fail, \
                            failure_hash, streak, streak_since_sha";
const MAIN_UPDATE: &str = "run_id = EXCLUDED.run_id, received_at = EXCLUDED.received_at, \
                           received_on = EXCLUDED.received_on, sha = EXCLUDED.sha, \
                           is_fail = EXCLUDED.is_fail, failure_hash = EXCLUDED.failure_hash, \
                           streak = EXCLUDED.streak, streak_since_sha = EXCLUDED.streak_since_sha";

/// `rows` (one test's main results in order, each `repo, test_id, run_id,
/// received_at, received_on, sha, is_fail, failure_hash, is_seed,
/// seed_streak, seed_since`) with, per row: `streak`, the failures with the
/// same non-null hash that end at it (a seed row brings its own count),
/// `since`, the sha that streak began at, the previous row's `prev_run_id`,
/// `prev_is_fail` and `prev_streak`, and `is_last`.
fn streaks(rows: &str) -> String {
    format!(
        "(WITH s0 AS ( \
              SELECT *, (is_fail AND failure_hash IS NOT NULL) AS cont FROM ({rows}) x), \
          s1 AS ( \
              SELECT *, CASE WHEN cont AND COALESCE(lag(cont) OVER k, false) \
                                  AND lag(failure_hash) OVER k = failure_hash \
                             THEN 0 ELSE 1 END AS starts \
              FROM s0 WINDOW k AS (PARTITION BY repo, test_id ORDER BY received_at, run_id)), \
          s2 AS ( \
              SELECT *, sum(starts) OVER (PARTITION BY repo, test_id \
                                          ORDER BY received_at, run_id \
                                          ROWS UNBOUNDED PRECEDING) AS island \
              FROM s1), \
          s3 AS ( \
              SELECT *, \
                     CASE WHEN cont THEN (sum(CASE WHEN is_seed THEN seed_streak ELSE 1 END) \
                                          OVER i)::int ELSE 0 END AS streak, \
                     CASE WHEN cont THEN first_value(CASE WHEN is_seed THEN seed_since \
                                                          ELSE sha END) OVER i END AS since \
              FROM s2 WINDOW i AS (PARTITION BY repo, test_id, island \
                                   ORDER BY received_at, run_id ROWS UNBOUNDED PRECEDING)) \
          SELECT repo, test_id, run_id, received_at, received_on, sha, is_fail, failure_hash, \
                 is_seed, streak, since, \
                 lag(run_id) OVER k AS prev_run_id, lag(is_fail) OVER k AS prev_is_fail, \
                 lag(received_at) OVER k AS prev_received_at, \
                 lag(received_on) OVER k AS prev_received_on, \
                 lag(streak) OVER k AS prev_streak, \
                 row_number() OVER (PARTITION BY repo, test_id \
                                    ORDER BY received_at DESC, run_id DESC) = 1 AS is_last \
          FROM s3 WINDOW k AS (PARTITION BY repo, test_id ORDER BY received_at, run_id))"
    )
}

/// Refill `test_main` from the results: each test's last default-branch
/// result up to `window.from` and its streak, within the retention period.
async fn rebuild_main(
    transaction: &tokio_postgres::Transaction<'_>,
    window: &Window,
) -> Result<()> {
    transaction
        .batch_execute("DELETE FROM test_results.test_main")
        .await?;
    let finals = format!(
        "SELECT DISTINCT ON (tr.run_id, tr.test_id) \
                tr.repo, tr.test_id, tr.run_id, r.received_at, tr.received_on, r.sha, \
                tr.outcome IN ('fail', 'error') AS is_fail, tr.failure_hash, \
                false AS is_seed, 0 AS seed_streak, NULL::text AS seed_since \
         FROM test_results.test_results tr \
         JOIN test_results.runs r ON r.run_id = tr.run_id \
         JOIN test_results.target_runs t \
              ON t.run_id = tr.run_id AND t.address = tr.target \
             AND t.received_on = tr.received_on \
         WHERE tr.received_on BETWEEN {history} AND {through_day} \
           AND r.received_at <= {through} AND {main} \
           AND tr.outcome <> 'skip' AND NOT t.cached \
           AND t.confidence IS DISTINCT FROM 'partial' \
         ORDER BY tr.run_id, tr.test_id, tr.attempt DESC",
        history = day(window.history_from),
        through_day = day(window.from.date_naive()),
        through = at(window.from),
        main = window.branch_is_default("r.branch"),
    );
    transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_main ({MAIN_COLUMNS}) \
                 SELECT repo, test_id, run_id, received_at, received_on, sha, is_fail, \
                        failure_hash, streak, since \
                 FROM {} s WHERE is_last",
                streaks(&finals)
            ),
            &[],
        )
        .await?;
    Ok(())
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
