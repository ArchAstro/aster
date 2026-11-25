#![allow(clippy::disallowed_macros)] // test diagnostics may use std print macros
//! Rebuilding `test_results.test_main`, each test's carried main sequence,
//! from a long history.
//!
//! v0.37.0 rebuilt it in one statement that sorted every retained
//! main-sequence result; with `ASTER_SRV_REFERENCE_WORKFLOWS` turned on in
//! production every pass failed on Cloud SQL's 5 GB `temp_file_limit` and
//! rolled back. These tests generate a month of history, rebuild it the way
//! the classifier does now (in steps of a few hours of results), and check:
//!
//! - the result equals what the one statement built ([`one_statement`]),
//!   after a rule change and after `main_through` was lost;
//! - it fits under a temporary-file cap that the one statement exceeds;
//! - a swap that fails leaves passes going under the old rule;
//! - a step too large for the cap is retried shorter, and later ticks start
//!   at the span that worked;
//! - runs on a step's boundary are counted once;
//! - passes moving the window between steps, and an old replica's passes,
//!   do not change the result;
//! - a rebuild left when the setting is set back resumes, and retention
//!   trims `test_main_rebuild`.
//!
//! The ignored `rebuild_scale` measures a production-sized history:
//! `cargo test -p aster-srv --test main_rebuild -- --ignored --nocapture
//! rebuild_scale` with `ASTER_SRV_TEST_DATABASE_URL` set. `BENCH_DAYS`,
//! `BENCH_TESTS`, `BENCH_MINUTES` (between main runs) and `BENCH_PRS` (pull
//! request runs per main run) size it; the default is 30 days of 20,000
//! tests every 26 minutes, about 22 million Stable CI results.

mod common;

use std::time::Instant;

use aster_srv::test_results::{classifier, ensure_partitions, retention};
use chrono::{DateTime, Duration, DurationRound, NaiveDate, Utc};
use common::{start, Server};

const REPO: &str = "ArchAstro/history";
const STABLE: &str = "Stable CI";
/// The classifier's overlap: a pass leaves `test_main` this far before its
/// clock.
const OVERLAP: Duration = Duration::minutes(15);

/// The shape of a generated history.
struct History {
    days: i64,
    /// Minutes between main runs; each slot also has `prs` pull request runs.
    minutes: i64,
    tests: i64,
    prs: i64,
}

/// Fill the database with `history` ending at `end`.
///
/// Every slot has one default-branch push of `Stable CI` (two in three),
/// `Nightly` or no workflow name (one in ten), a seventh of them stored with
/// no source (judged by branch), and `prs` pull request runs. Eight targets;
/// some target runs are cached or partial. Tests fall in eight classes by
/// `t % 8`: long same-hash failure streaks whose hash changes every 20
/// slots, coin flips, failures with and without a hash, steady passes,
/// almost always failing, often skipped, retried, and rare failures. A
/// failing attempt is retried and passes one time in three.
async fn seed_history(server: &Server, end: DateTime<Utc>, history: &History) -> i64 {
    let client = server.client().await;
    let first = end - Duration::days(history.days);
    let mut day = first.date_naive();
    while day <= end.date_naive() {
        ensure_partitions(&client, day).await.unwrap();
        day = day.succ_opt().unwrap();
    }
    let slots = history.days * 24 * 60 / history.minutes;
    client
        .batch_execute(&format!(
            "CREATE TABLE gen_runs AS \
             SELECT n, k, 'gen-' || n || '-' || k AS run_id, \
                    timestamptz '{first}' + n * interval '{minutes} minutes' \
                        + k * interval '7 seconds' AS at, \
                    k = 0 AS main, \
                    CASE WHEN k > 0 OR n % 10 = 9 THEN NULL \
                         WHEN n % 3 = 2 THEN 'Nightly' ELSE '{STABLE}' END AS workflow, \
                    CASE WHEN k = 0 AND n % 7 = 3 THEN NULL ELSE 'oidc' END AS source \
             FROM generate_series(1, {slots}) n, generate_series(0, {prs}) k; \
             INSERT INTO test_results.runs \
                 (run_id, repo, sha, branch, workflow, aster_version, started_at, received_at, \
                  source, git_ref, event_name) \
             SELECT run_id, '{REPO}', 'sha-' || n || '-' || k, \
                    CASE WHEN main THEN 'main' ELSE 'pr-' || k END, workflow, '0', at, at, \
                    source, \
                    CASE WHEN source IS NULL THEN NULL WHEN main THEN 'refs/heads/main' \
                         ELSE 'refs/pull/' || k || '/merge' END, \
                    CASE WHEN source IS NULL THEN NULL WHEN main THEN 'push' \
                         ELSE 'pull_request' END \
             FROM gen_runs; \
             INSERT INTO test_results.target_runs \
                 (received_on, run_id, repo, address, input_hash, status, cached, duration_ms, \
                  confidence) \
             SELECT (at AT TIME ZONE 'UTC')::date, run_id, '{REPO}', '//t' || g, \
                    'h' || g || '-' || (n / 3), 'passed', \
                    abs(hashint8(n * 31 + g + k * 7)) % 13 = 0, 1, \
                    CASE WHEN abs(hashint8(n * 17 + g + k)) % 17 = 0 \
                         THEN 'partial' ELSE 'complete' END \
             FROM gen_runs, generate_series(0, 7) g; \
             INSERT INTO test_results.tests (repo, test_id, target, file, name, first_seen, last_seen) \
             SELECT '{REPO}', 't' || t, '//t' || (t % 8), 'test/t' || t, 't' || t, \
                    timestamptz '{first}', timestamptz '{end}' \
             FROM generate_series(1, {tests}) t;",
            minutes = history.minutes,
            prs = history.prs,
            tests = history.tests,
        ))
        .await
        .unwrap();
    // A day at a time, so one statement stays a day's size. In run order,
    // as uploads store them (one `COPY` per run), not interleaved.
    let mut day = first.date_naive();
    while day <= end.date_naive() {
        client
            .batch_execute(&format!(
                "INSERT INTO test_results.test_results \
                     (received_on, run_id, repo, target, test_id, outcome, attempt, duration_ms, \
                      failure_hash) \
                 SELECT (r.at AT TIME ZONE 'UTC')::date, r.run_id, '{REPO}', '//t' || (t % 8), \
                        't' || t, \
                        CASE WHEN a.attempt = 2 THEN 'pass' \
                             WHEN o.skip THEN 'skip' WHEN o.fail THEN 'fail' ELSE 'pass' END, \
                        a.attempt, 5, \
                        CASE WHEN a.attempt = 1 AND o.fail AND NOT o.skip THEN \
                             CASE t % 8 WHEN 0 THEN 'a' || ((r.n / 20) % 2) \
                                        WHEN 2 THEN CASE WHEN o.p % 2 = 0 THEN 'n' END \
                                        WHEN 4 THEN 'z' ELSE 'h' || (t % 8) END END \
                 FROM gen_runs r CROSS JOIN generate_series(1, {tests}) t \
                 CROSS JOIN LATERAL (SELECT abs(hashint8(r.n * 100003 + t + r.k * 7919)) % 100 AS p) h \
                 CROSS JOIN LATERAL ( \
                     SELECT h.p AS p, \
                            (t % 8 = 5 AND h.p < 30) OR h.p = 99 AS skip, \
                            h.p < (ARRAY[85, 50, 30, 0, 95, 20, 40, 5])[t % 8 + 1] AS fail) o \
                 CROSS JOIN LATERAL (VALUES (1), (2)) a (attempt) \
                 WHERE (r.at AT TIME ZONE 'UTC')::date = '{day}' \
                   AND (a.attempt = 1 OR (o.fail AND NOT o.skip AND o.p % 3 = 0)) \
                 ORDER BY r.n, r.k, t, a.attempt",
                tests = history.tests,
            ))
            .await
            .unwrap();
        day = day.succ_opt().unwrap();
    }
    client
        .batch_execute("DROP TABLE gen_runs; ANALYZE")
        .await
        .unwrap();
    client
        .query_one("SELECT count(*) FROM test_results.test_results", &[])
        .await
        .unwrap()
        .get(0)
}

/// v0.37.0's `rebuild_main`: each test's last main-sequence result up to
/// `through` within the retention period, and its failure streak, in one
/// statement over every retained result. `main` is the main-sequence rule
/// over the run `r`. Kept here verbatim as the reference the stepwise
/// rebuild must equal.
fn one_statement(history: NaiveDate, through: DateTime<Utc>, main: &str) -> String {
    let through_day = through.date_naive();
    let through = through.to_rfc3339();
    format!(
        "WITH s0 AS ( \
              SELECT *, (is_fail AND failure_hash IS NOT NULL) AS cont FROM ( \
                  SELECT DISTINCT ON (tr.run_id, tr.test_id) \
                         tr.repo, tr.test_id, tr.run_id, r.received_at, tr.received_on, r.sha, \
                         tr.outcome IN ('fail', 'error') AS is_fail, tr.failure_hash, \
                         false AS is_seed, 0 AS seed_streak, NULL::text AS seed_since \
                  FROM test_results.test_results tr \
                  JOIN test_results.runs r ON r.run_id = tr.run_id \
                  JOIN test_results.target_runs t \
                       ON t.run_id = tr.run_id AND t.address = tr.target \
                      AND t.received_on = tr.received_on \
                  WHERE tr.received_on BETWEEN '{history}'::date AND '{through_day}'::date \
                    AND r.received_at <= '{through}'::timestamptz AND {main} \
                    AND tr.outcome <> 'skip' AND NOT t.cached \
                    AND t.confidence IS DISTINCT FROM 'partial' \
                  ORDER BY tr.run_id, tr.test_id, tr.attempt DESC) x), \
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
                                   ORDER BY received_at, run_id ROWS UNBOUNDED PRECEDING)), \
          s AS ( \
              SELECT *, row_number() OVER (PARTITION BY repo, test_id \
                                           ORDER BY received_at DESC, run_id DESC) = 1 AS is_last \
              FROM s3) \
         SELECT repo, test_id, run_id, received_at, received_on, sha, is_fail, failure_hash, \
                streak, since \
         FROM s WHERE is_last"
    )
}

/// The main-sequence rule over the run `r` for the default branch `main`,
/// with `workflow` as the only reference workflow when set.
fn main_rule(workflow: Option<&str>) -> String {
    let default = "(CASE WHEN r.source = 'oidc' \
                         THEN COALESCE(r.git_ref = 'refs/heads/main' \
                                       AND r.event_name IN ('push', 'workflow_dispatch', 'schedule'), \
                                       false) \
                         ELSE COALESCE(r.branch IN ('main', 'refs/heads/main'), false) END)";
    match workflow {
        None => default.to_string(),
        Some(workflow) => format!("({default} AND COALESCE(r.workflow IN ('{workflow}'), false))"),
    }
}

/// A `test_main` row as text, for comparing whole tables.
type MainRow = String;

const MAIN_ROW: &str = "concat_ws('|', repo, test_id, run_id, received_at, received_on, sha, \
                        is_fail, failure_hash, streak, since)";

/// Settings for running [`one_statement`] as a reference: no temporary
/// file cap, and no nested loops (on small histories the planner joins
/// each target run's results by index, which takes minutes).
const UNCAPPED: &str = "SET LOCAL temp_file_limit = -1; SET LOCAL enable_nestloop = off";

/// What [`one_statement`] builds, sorted.
async fn expected(
    server: &Server,
    history: NaiveDate,
    through: DateTime<Utc>,
    main: &str,
) -> Vec<MainRow> {
    let mut client = server.client().await;
    let transaction = client.transaction().await.unwrap();
    transaction.batch_execute(UNCAPPED).await.unwrap();
    let rows = transaction
        .query(
            &format!(
                "SELECT {MAIN_ROW} FROM ({}) m ORDER BY repo, test_id",
                one_statement(history, through, main)
            ),
            &[],
        )
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    rows.iter().map(|row| row.get(0)).collect()
}

/// `test_main` as the classifier left it, sorted.
async fn carried(server: &Server) -> Vec<MainRow> {
    table(server, "test_main").await
}

/// `test_main_rebuild` as far as the rebuild got, sorted.
async fn rebuilt_so_far(server: &Server) -> Vec<MainRow> {
    table(server, "test_main_rebuild").await
}

async fn table(server: &Server, table: &str) -> Vec<MainRow> {
    server
        .client()
        .await
        .query(
            &format!(
                "SELECT {} FROM test_results.{table} ORDER BY repo, test_id",
                MAIN_ROW.replace("since", "streak_since_sha")
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// Record that every run up to `now` was classified under the rule `main`
/// (`workflows` as `classifier_state` stores it), as a pass of v0.37.0 at
/// `now` leaves it: `test_main` up to `now - OVERLAP`.
async fn classified_by_an_earlier_release(
    server: &Server,
    now: DateTime<Utc>,
    history: NaiveDate,
    main: &str,
    workflows: Option<&str>,
) {
    let through = now - OVERLAP;
    let client = server.client().await;
    client
        .batch_execute(&format!(
            "BEGIN; {UNCAPPED}; \
             INSERT INTO test_results.test_main \
                 (repo, test_id, run_id, received_at, received_on, sha, is_fail, failure_hash, \
                  streak, streak_since_sha) \
             {}; \
             COMMIT;",
            one_statement(history, through, main)
        ))
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO test_results.classifier_state \
                 (id, watermark, main_through, main_workflows, main_workflows_through) \
             VALUES (1, $1, $2, $3, $2)",
            &[&now, &through, &workflows],
        )
        .await
        .unwrap();
}

/// `classifier_state`'s rule and rebuild columns.
async fn rebuild_state(
    server: &Server,
) -> (Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let row = server
        .client()
        .await
        .query_one(
            "SELECT main_workflows, main_through, rebuild_through FROM test_results.classifier_state",
            &[],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1), row.get(2))
}

fn history_day(now: DateTime<Utc>) -> NaiveDate {
    // `ASTER_SRV_RETENTION_DAYS`' default.
    now.date_naive() - chrono::Days::new(30)
}

/// The retention period of the tests below that run many ticks: four
/// rebuild steps a day.
const SHORT_RETENTION: &str = "3";

/// The first day kept under [`SHORT_RETENTION`].
fn short_history_day(now: DateTime<Utc>) -> NaiveDate {
    now.date_naive() - chrono::Days::new(3)
}

/// A clock for tests that move it forward by up to an hour: never past
/// 23:00 UTC, so the retention period's first day does not change under
/// them.
fn clock() -> DateTime<Utc> {
    let now = Utc::now().duration_trunc(Duration::minutes(1)).unwrap();
    let eleven = now
        .date_naive()
        .and_hms_opt(23, 0, 0)
        .expect("23:00")
        .and_utc();
    now.min(eleven)
}

/// One classifier tick at `now`, which must not fail.
async fn tick(server: &Server, now: DateTime<Utc>) -> classifier::Report {
    classifier::run_once(&server.app, now)
        .await
        .expect("the tick does not fail")
        .expect("no other replica holds the lock")
}

/// A Stable CI push to main received at `at`, of `tests` tests in eight
/// targets: a test fails with `failure` (an SQL expression over the test
/// number `t`, NULL to pass).
async fn main_run(server: &Server, run_id: &str, at: DateTime<Utc>, tests: i64, failure: &str) {
    let client = server.client().await;
    ensure_partitions(&client, at.date_naive()).await.unwrap();
    client
        .batch_execute(&format!(
            "INSERT INTO test_results.runs \
                 (run_id, repo, sha, branch, workflow, aster_version, started_at, received_at, \
                  source, git_ref, event_name) \
             VALUES ('{run_id}', '{REPO}', 'sha-{run_id}', 'main', '{STABLE}', '0', \
                     '{at}', '{at}', 'oidc', 'refs/heads/main', 'push'); \
             INSERT INTO test_results.target_runs \
                 (received_on, run_id, repo, address, input_hash, status, cached, duration_ms, \
                  confidence) \
             SELECT '{day}', '{run_id}', '{REPO}', '//t' || g, 'x-{run_id}', 'passed', false, 1, \
                    'complete' \
             FROM generate_series(0, 7) g; \
             INSERT INTO test_results.test_results \
                 (received_on, run_id, repo, target, test_id, outcome, attempt, duration_ms, \
                  failure_hash) \
             SELECT '{day}', '{run_id}', '{REPO}', '//t' || (t % 8), 't' || t, \
                    CASE WHEN f IS NULL THEN 'pass' ELSE 'fail' END, 1, 5, f \
             FROM generate_series(1, {tests}) t, LATERAL (SELECT {failure} AS f) x;",
            at = at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            day = at.date_naive(),
        ))
        .await
        .unwrap();
}

/// A history under [`SHORT_RETENTION`] classified by an earlier release
/// under the every-workflow rule up to `now`: four days (one past the
/// retention period) of a main run every 40 minutes beside a pull request
/// run, over 48 tests.
async fn short_history(env: &[(&str, &str)]) -> Option<(Server, DateTime<Utc>, NaiveDate)> {
    let mut env = env.to_vec();
    env.push(("ASTER_SRV_RETENTION_DAYS", SHORT_RETENTION));
    let server = start(&env).await?;
    let now = clock();
    seed_history(
        &server,
        now - Duration::minutes(20),
        &History {
            days: 4,
            minutes: 40,
            tests: 48,
            prs: 1,
        },
    )
    .await;
    let history = short_history_day(now);
    classified_by_an_earlier_release(&server, now, history, &main_rule(None), None).await;
    Some((server, now, history))
}

/// `classifier_state.watermark`.
async fn watermark(server: &Server) -> DateTime<Utc> {
    server
        .client()
        .await
        .query_one("SELECT watermark FROM test_results.classifier_state", &[])
        .await
        .unwrap()
        .get(0)
}

/// Log capture is one buffer for the whole test binary, so the tests here
/// run one at a time and read only the lines logged since they started.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Where the captured log is now; see [`logged_since`].
fn log_mark() -> usize {
    aster_srv::log::start_capture();
    aster_srv::log::captured().len()
}

/// The captured lines after `mark` whose message contains `message`.
fn logged_since(mark: usize, message: &str) -> Vec<String> {
    aster_srv::log::captured()
        .into_iter()
        .skip(mark)
        .filter(|line| line.contains(&format!("\"message\":\"{message}")))
        .collect()
}

/// No rebuild step or swap failed since `mark` (a statement over the cap
/// would).
fn no_step_failed(mark: usize) {
    let failed: Vec<String> = aster_srv::log::captured()
        .into_iter()
        .skip(mark)
        .filter(|line| line.contains("rebuild") && line.contains("failed"))
        .collect();
    assert!(failed.is_empty(), "rebuild steps failed: {failed:?}");
}

#[tokio::test]
async fn a_rebuild_in_steps_carries_the_same_streaks_as_the_one_statement() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: 33 days of history, past the 30 retained, a main run every 40
    // minutes with a pull request run beside it. Streaks run across the
    // rebuild's six-hour steps and across the retention boundary.
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now().duration_trunc(Duration::minutes(1)).unwrap();
    let end = now - Duration::minutes(20);
    seed_history(
        &server,
        end,
        &History {
            days: 33,
            minutes: 40,
            tests: 48,
            prs: 1,
        },
    )
    .await;
    let history = history_day(now);
    let through = now - OVERLAP;

    // An earlier release classified everything with every default-branch
    // workflow in the main sequence.
    classified_by_an_earlier_release(&server, now, history, &main_rule(None), None).await;
    let before = carried(&server).await;
    assert_eq!(before.len(), 48, "every test has a main-sequence result");

    // Action: the server restarts with Stable CI as the reference workflow,
    // and one pass runs at the same clock. It goes on under the old rule,
    // rebuilds in steps, and swaps the rebuild in.
    let stable = server
        .second(&[("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE)])
        .await;
    classifier::run_once(&stable.app, now)
        .await
        .unwrap()
        .unwrap();

    // Outcome: exactly what the one statement builds for Stable CI, which
    // differs from what the old rule carried.
    let rebuilt = carried(&stable).await;
    assert_eq!(
        rebuilt,
        expected(&stable, history, through, &main_rule(Some(STABLE))).await
    );
    assert_ne!(rebuilt, before);
    assert_eq!(
        rebuild_state(&stable).await,
        (Some(r#"["Stable CI"]"#.into()), Some(through), None)
    );
    stable.stop().await;

    // Action: `main_through` is lost (as after an upgrade from a release
    // that did not keep it), and the server runs without reference
    // workflows. The pass waits for the rebuild, which reads everything.
    server
        .client()
        .await
        .batch_execute("UPDATE test_results.classifier_state SET main_through = NULL")
        .await
        .unwrap();
    let every = server
        .second(&[("ASTER_SRV_REFERENCE_WORKFLOWS", "")])
        .await;
    classifier::run_once(&every.app, now)
        .await
        .unwrap()
        .unwrap();

    // Outcome: what the one statement builds for every workflow.
    assert_eq!(carried(&every).await, before);
    assert_eq!(rebuild_state(&every).await, (None, Some(through), None));
    every.stop().await;
    no_step_failed(mark);
    server.stop().await;
}

/// Cap every new connection to `server`'s database at `limit` of temporary
/// files, with production's `work_mem`.
async fn cap_temp_files(server: &Server, limit: &str) {
    let client = server.client().await;
    let database: String = client
        .query_one("SELECT current_database()", &[])
        .await
        .unwrap()
        .get(0);
    client
        .batch_execute(&format!(
            "ALTER DATABASE {database} SET temp_file_limit = '{limit}'; \
             ALTER DATABASE {database} SET work_mem = '4MB'"
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_rebuild_fits_under_a_temp_file_cap_the_one_statement_exceeds() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: 30 days of a main run every 15 minutes over 48 tests, about
    // 70,000 main-sequence results under Stable CI, classified by an earlier
    // release under the every-workflow rule.
    let Some(server) = start(&[]).await else {
        return;
    };
    let now = Utc::now().duration_trunc(Duration::minutes(1)).unwrap();
    seed_history(
        &server,
        now - Duration::minutes(20),
        &History {
            days: 30,
            minutes: 15,
            tests: 48,
            prs: 0,
        },
    )
    .await;
    let history = history_day(now);
    let through = now - OVERLAP;
    classified_by_an_earlier_release(&server, now, history, &main_rule(None), None).await;

    // The classifier's connections may use 4 MB of temporary files. The
    // one-statement rebuild needs more: under the cap it fails as it did in
    // production (and this history is large enough to show it).
    cap_temp_files(&server, "4MB").await;
    let stable = server
        .second(&[("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE)])
        .await;
    let error = stable
        .client()
        .await
        .query(
            &format!(
                "SELECT count(*) FROM ({}) m",
                one_statement(history, through, &main_rule(Some(STABLE)))
            ),
            &[],
        )
        .await
        .expect_err("the one statement exceeds the cap");
    assert!(
        format!("{error:?}").contains("temp_file_limit"),
        "{error:?}"
    );

    // Action: the classifier's pass after the rule change, under the cap.
    let report = classifier::run_once(&stable.app, now)
        .await
        .expect("no statement of the pass or the rebuild exceeds the cap")
        .unwrap();
    assert_eq!(report.results, 0, "no run arrived in the last 15 minutes");

    // Outcome: rebuilt and swapped in, the same as the one statement.
    no_step_failed(mark);
    assert_eq!(rebuild_state(&stable).await.2, None);
    assert_eq!(
        carried(&stable).await,
        expected(&stable, history, through, &main_rule(Some(STABLE))).await
    );
    stable.stop().await;
    server.stop().await;
}

fn env_or(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Temporary bytes written in `server`'s database so far, once the
/// statistics have been flushed.
async fn temp_bytes(server: &Server) -> i64 {
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    server
        .client()
        .await
        .query_one(
            "SELECT temp_bytes FROM pg_stat_database WHERE datname = current_database()",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Run passes at `now` the way the every-five-minutes loop does, until
/// `done` says the rebuild was swapped in; each spends at most two minutes
/// on it. Prints what it took.
async fn ticks_until(
    server: &Server,
    now: DateTime<Utc>,
    what: &str,
    done: impl Fn(&(Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>)) -> bool,
) {
    let mark = log_mark();
    let before = temp_bytes(server).await;
    let started = Instant::now();
    let mut ticks = 0;
    while !done(&rebuild_state(server).await) {
        assert!(ticks < 60, "the rebuild does not finish");
        classifier::run_once(&server.app, now)
            .await
            .unwrap()
            .unwrap();
        ticks += 1;
    }
    let seconds = started.elapsed().as_secs_f64();
    let written = temp_bytes(server).await - before;
    no_step_failed(mark);
    println!(
        "{what}: rebuilt and swapped in over {ticks} ticks, {seconds:.1}s of work, \
         {:.1} MB of temporary files written in all",
        written as f64 / 1e6
    );
}

#[tokio::test]
#[ignore]
async fn rebuild_scale() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    let Some(server) = start(&[]).await else {
        return;
    };
    let history = History {
        days: env_or("BENCH_DAYS", 30),
        minutes: env_or("BENCH_MINUTES", 26),
        tests: env_or("BENCH_TESTS", 20_000),
        prs: env_or("BENCH_PRS", 0),
    };
    let now = Utc::now().duration_trunc(Duration::minutes(1)).unwrap();
    let started = Instant::now();
    let results = seed_history(&server, now - Duration::minutes(20), &history).await;
    let day = history_day(now);
    let through = now - OVERLAP;
    let counts = server
        .client()
        .await
        .query_one(
            &format!(
                "SELECT count(*) FILTER (WHERE {every}), count(*) FILTER (WHERE {stable}) \
                 FROM test_results.test_results tr \
                 JOIN test_results.runs r ON r.run_id = tr.run_id \
                 WHERE tr.received_on >= '{day}'::date",
                every = main_rule(None),
                stable = main_rule(Some(STABLE)),
            ),
            &[],
        )
        .await
        .unwrap();
    let (every, stable): (i64, i64) = (counts.get(0), counts.get(1));
    println!(
        "seeded {results} results in {:.0}s ({} days, {} tests, a main run every {} minutes, \
         {} pull request runs each); retained default-branch results: {every}, of them \
         Stable CI: {stable}; database {}",
        started.elapsed().as_secs_f64(),
        history.days,
        history.tests,
        history.minutes,
        history.prs,
        server
            .client()
            .await
            .query_one(
                "SELECT pg_size_pretty(pg_database_size(current_database()))",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
    );
    // Flake events, which the pass that swaps a rebuild in reads for every
    // test: `BENCH_EVENTS` a day.
    server
        .client()
        .await
        .batch_execute(&format!(
            "INSERT INTO test_results.test_flake_events \
                 (observed_on, repo, test_id, kind, reason, run_id, target, observed_at) \
             SELECT d::date, '{REPO}', 't' || (1 + abs(hashint8(i)) % {tests}), \
                    CASE WHEN i % 3 = 0 THEN 'definite' ELSE 'likely' END, 'retry', \
                    'ev-' || i, '//t0', d + (i % 86400) * interval '1 second' \
             FROM generate_series('{first}'::date, '{today}'::date - 1, interval '1 day') d, \
                  generate_series(1, {events}) i; \
             ANALYZE test_results.test_flake_events",
            tests = history.tests,
            first = day.max((now - Duration::days(history.days)).date_naive()),
            today = now.date_naive(),
            events = env_or("BENCH_EVENTS", 50_000),
        ))
        .await
        .unwrap();
    // Classified up to `now` by a release that kept no `main_through` (the
    // first rebuild reads everything), with no reference workflows.
    server
        .client()
        .await
        .execute(
            "INSERT INTO test_results.classifier_state (id, watermark) VALUES (1, $1)",
            &[&now],
        )
        .await
        .unwrap();

    // Every statement the classifier runs may use 1 GB of temporary files,
    // a fifth of production's limit. Each temporary file it writes is
    // logged with its size and statement.
    cap_temp_files(&server, "1GB").await;
    server
        .client()
        .await
        .batch_execute(
            "DO $$ BEGIN EXECUTE format('ALTER DATABASE %I SET log_temp_files = 0', \
                                        current_database()); END $$",
        )
        .await
        .unwrap();

    // `main_through` lost: passes wait for the rebuild.
    let every_workflow = server
        .second(&[("ASTER_SRV_REFERENCE_WORKFLOWS", "")])
        .await;
    ticks_until(&every_workflow, now, "main_through lost", |state| {
        state.1.is_some()
    })
    .await;
    every_workflow.stop().await;

    // The rule changes to Stable CI: passes go on under the old one.
    let stable_ci = server
        .second(&[("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE)])
        .await;
    ticks_until(&stable_ci, now, "rule change to Stable CI", |state| {
        state.0.is_some()
    })
    .await;
    for line in aster_srv::log::captured().into_iter().skip(mark) {
        if line.contains("rebuild") {
            println!("  {line}");
        }
    }

    // The one statement over the same history, without a cap, for what it
    // needs and to check the result.
    let before = temp_bytes(&server).await;
    let started = Instant::now();
    let reference = expected(&server, day, through, &main_rule(Some(STABLE))).await;
    println!(
        "one statement (Stable CI): {:.1}s, {:.1} MB of temporary files",
        started.elapsed().as_secs_f64(),
        (temp_bytes(&server).await - before) as f64 / 1e6
    );
    assert_eq!(carried(&stable_ci).await, reference);
    println!(
        "test_main equals the one statement's ({} tests)",
        reference.len()
    );
    stable_ci.stop().await;
    server.stop().await;
}

#[tokio::test]
async fn a_swap_that_fails_leaves_passes_classifying_under_the_old_rule() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: a history classified under the every-workflow rule; the server
    // restarts with Stable CI as the reference workflow, one rebuild step a
    // tick. The first tick starts the rebuild.
    let Some((server, now, history)) = short_history(&[]).await else {
        return;
    };
    let stable = server
        .second(&[
            ("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE),
            ("ASTER_SRV_REBUILD_BUDGET_SECONDS", "0"),
        ])
        .await;
    tick(&stable, now).await;
    assert!(
        rebuild_state(&stable).await.2.is_some(),
        "a rebuild is under way"
    );

    // Fault: swapping the rebuild in fails. A swap empties
    // `test_main_rebuild`; nothing else does once a rebuild is under way.
    stable
        .client()
        .await
        .batch_execute(
            "CREATE FUNCTION test_results.fail_swap() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RAISE EXCEPTION 'swap fault'; END $$; \
             CREATE TRIGGER fail_swap BEFORE DELETE ON test_results.test_main_rebuild \
                 FOR EACH STATEMENT EXECUTE FUNCTION test_results.fail_swap();",
        )
        .await
        .unwrap();

    // Action: ticks at the same clock until the rebuild has caught up and a
    // swap was tried. Each tick succeeds, under the old rule.
    let mut ticks = 0;
    while logged_since(mark, "test_main rebuild swap failed").is_empty() {
        assert!(ticks < 40, "the rebuild does not catch up");
        tick(&stable, now).await;
        assert_eq!(rebuild_state(&stable).await.0, None, "still the old rule");
        ticks += 1;
    }

    // Action: a main run arrives, and two more ticks run with the fault
    // still there. The first pass of each plans the swap, which fails; the
    // pass after it classifies the run under the old rule.
    main_run(
        &stable,
        "late-1",
        now - Duration::minutes(8),
        48,
        "CASE WHEN t % 2 = 0 THEN 'late' END",
    )
    .await;
    let later = now + Duration::minutes(5);
    let report = tick(&stable, later).await;
    assert_eq!(report.results, 48, "the late run's results are classified");
    assert_eq!(watermark(&stable).await, later);
    assert_eq!(rebuild_state(&stable).await.0, None, "still the old rule");
    let later = now + Duration::minutes(10);
    tick(&stable, later).await;
    assert_eq!(watermark(&stable).await, later);
    assert_eq!(rebuild_state(&stable).await.0, None, "still the old rule");

    // Action: the fault is removed. The next tick swaps the rebuild in.
    stable
        .client()
        .await
        .batch_execute("DROP TRIGGER fail_swap ON test_results.test_main_rebuild")
        .await
        .unwrap();
    let last = now + Duration::minutes(15);
    tick(&stable, last).await;

    // Outcome: the new rule, and what the one statement builds for it,
    // late run included.
    let through = last - OVERLAP;
    assert_eq!(
        rebuild_state(&stable).await,
        (Some(r#"["Stable CI"]"#.into()), Some(through), None)
    );
    assert_eq!(
        carried(&stable).await,
        expected(&stable, history, through, &main_rule(Some(STABLE))).await
    );
    stable.stop().await;
    server.stop().await;
}

#[tokio::test]
async fn steps_that_end_on_a_run_carry_the_same_streaks_as_the_one_statement() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: a history classified under the every-workflow rule.
    let Some((server, now, history)) = short_history(&[]).await else {
        return;
    };
    // The rebuild's first step starts just before the first run kept and
    // each reads six hours (`(from, to]`), so steps end at `first - 1µs +
    // 6h·k`. Main runs land on the end of the first two steps and one
    // microsecond after. (The generated history also has a run one
    // microsecond after each step's end.)
    let client = server.client().await;
    let first: DateTime<Utc> = client
        .query_one(
            "SELECT min(received_at) FROM test_results.runs WHERE received_at >= $1",
            &[&history.and_hms_opt(0, 0, 0).unwrap().and_utc()],
        )
        .await
        .unwrap()
        .get(0);
    let micro = Duration::microseconds(1);
    let first_end = first - micro + Duration::hours(6);
    let second_end = first_end + Duration::hours(6);
    // Tests 49 to 60 run only in these four runs and fail in all of them
    // the same way: their streak, four long, runs through both ends.
    for (run, at, others) in [
        ("edge-1a", first_end, "t % 5 <> 0"),
        ("edge-1b", first_end + micro, "t % 3 <> 0"),
        ("edge-2a", second_end, "t % 4 <> 0"),
        ("edge-2b", second_end + micro, "t % 7 <> 0"),
    ] {
        main_run(
            &server,
            run,
            at,
            60,
            &format!("CASE WHEN t > 48 OR {others} THEN 'edge' END"),
        )
        .await;
    }
    client.batch_execute("ANALYZE").await.unwrap();

    // Action: the server restarts with Stable CI, one step a tick.
    let stable = server
        .second(&[
            ("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE),
            ("ASTER_SRV_REBUILD_BUDGET_SECONDS", "0"),
        ])
        .await;
    // The first two steps end exactly on the runs placed there, and what
    // each built so far is what the one statement builds up to its end:
    // the run on the end counted in that step, the one after it in the next.
    let stable_ci = main_rule(Some(STABLE));
    tick(&stable, now).await;
    assert_eq!(rebuild_state(&stable).await.2, Some(first_end));
    assert_eq!(
        rebuilt_so_far(&stable).await,
        expected(&stable, history, first_end, &stable_ci).await
    );
    tick(&stable, now).await;
    assert_eq!(rebuild_state(&stable).await.2, Some(second_end));
    assert_eq!(
        rebuilt_so_far(&stable).await,
        expected(&stable, history, second_end, &stable_ci).await
    );
    let mut ticks = 2;
    while rebuild_state(&stable).await.0.is_none() {
        assert!(ticks < 40, "the rebuild does not finish");
        tick(&stable, now).await;
        ticks += 1;
    }

    // Outcome: what the one statement builds, each boundary run counted once.
    assert_eq!(
        carried(&stable).await,
        expected(&stable, history, now - OVERLAP, &stable_ci).await
    );
    no_step_failed(mark);
    stable.stop().await;
    server.stop().await;
}

#[tokio::test]
async fn a_rebuild_over_many_ticks_while_passes_move_the_window_carries_the_same_streaks() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: a history classified under the every-workflow rule. Two
    // replicas: one restarted with Stable CI (one rebuild step a tick), one
    // still on the old configuration.
    let Some((server, now, history)) = short_history(&[]).await else {
        return;
    };
    let stable = server
        .second(&[
            ("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE),
            ("ASTER_SRV_REBUILD_BUDGET_SECONDS", "0"),
        ])
        .await;

    // Action: a tick every two minutes, with a main run arriving before
    // each. Every third tick is the old replica's: its pass moves the window
    // start (and `main_through`) between the rebuild's steps, and leaves the
    // rebuild alone. Each Stable CI tick's pass moves it too.
    let mut clock = now;
    let mut old_ticks_during_rebuild = 0;
    let mut k = 0;
    while rebuild_state(&stable).await.0.is_none() {
        assert!(k < 60, "the rebuild does not finish");
        k += 1;
        clock += Duration::minutes(2);
        main_run(
            &stable,
            &format!("moving-{k}"),
            clock - Duration::minutes(1),
            48,
            &format!("CASE WHEN (t + {k}) % 3 = 0 THEN 'm' || (t % 2) END"),
        )
        .await;
        if k % 3 == 0 {
            let before = rebuild_state(&server).await.2;
            tick(&server, clock).await;
            assert_eq!(
                rebuild_state(&server).await.2,
                before,
                "the old replica leaves the rebuild"
            );
            old_ticks_during_rebuild += 1;
        } else {
            tick(&stable, clock).await;
        }
    }
    assert!(k > 10, "the rebuild took {k} ticks");
    assert!(old_ticks_during_rebuild >= 3);

    // Outcome: what the one statement builds for Stable CI up to the last
    // pass, every moving run included.
    assert_eq!(
        carried(&stable).await,
        expected(&stable, history, clock - OVERLAP, &main_rule(Some(STABLE))).await
    );
    assert_eq!(logged_since(mark, "test_main rebuild starts").len(), 1);
    no_step_failed(mark);
    stable.stop().await;
    server.stop().await;
}

#[tokio::test]
async fn a_rebuild_left_when_the_rule_is_set_back_resumes_and_retention_trims_it() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: a history classified under the every-workflow rule; a replica
    // restarted with Stable CI makes three rebuild steps.
    let Some((server, now, history)) = short_history(&[]).await else {
        return;
    };
    let stable_env = [
        ("ASTER_SRV_REFERENCE_WORKFLOWS", STABLE),
        ("ASTER_SRV_REBUILD_BUDGET_SECONDS", "0"),
    ];
    let stable = server.second(&stable_env).await;
    for _ in 0..3 {
        tick(&stable, now).await;
    }
    let left = rebuild_state(&stable).await.2;
    assert!(left.is_some());
    stable.stop().await;

    // Action: the setting is set back before the rebuild finished. Passes
    // under the every-workflow rule go on and leave the rebuild as it is.
    for minutes in [5, 10] {
        tick(&server, now + Duration::minutes(minutes)).await;
    }
    assert_eq!(
        rebuild_state(&server).await,
        (None, Some(now - OVERLAP + Duration::minutes(10)), left)
    );

    // Action: Stable CI again. The rebuild goes on from where it was left,
    // not from the start, and is swapped in.
    let stable = server.second(&stable_env).await;
    let later = now + Duration::minutes(15);
    let mut ticks = 0;
    while rebuild_state(&stable).await.0.is_none() {
        assert!(ticks < 40, "the rebuild does not finish");
        tick(&stable, later).await;
        ticks += 1;
    }
    assert_eq!(
        logged_since(mark, "test_main rebuild starts").len(),
        1,
        "resumed"
    );
    assert_eq!(
        carried(&stable).await,
        expected(&stable, history, later - OVERLAP, &main_rule(Some(STABLE))).await
    );
    stable.stop().await;

    // Setup: `main_through` is lost and one step of a rebuild under the
    // every-workflow rule runs, so `test_main_rebuild` holds rows from the
    // retention period's first day.
    server
        .client()
        .await
        .batch_execute("UPDATE test_results.classifier_state SET main_through = NULL")
        .await
        .unwrap();
    let every = server
        .second(&[("ASTER_SRV_REBUILD_BUDGET_SECONDS", "0")])
        .await;
    tick(&every, later).await;
    let tomorrow = later.date_naive().succ_opt().unwrap();
    let cutoff = (tomorrow - chrono::Days::new(3))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let older = |table: &'static str| {
        let server = &every;
        async move {
            server
                .client()
                .await
                .query_one(
                    &format!("SELECT count(*) FROM test_results.{table} WHERE received_at < $1"),
                    &[&cutoff],
                )
                .await
                .unwrap()
                .get::<_, i64>(0)
        }
    };
    assert!(older("test_main_rebuild").await > 0);

    // Action: the next day's retention pass.
    retention::run_once(&every.app, tomorrow)
        .await
        .unwrap()
        .unwrap();

    // Outcome: neither table keeps a result from before the retention period.
    assert_eq!(older("test_main_rebuild").await, 0);
    assert_eq!(older("test_main").await, 0);
    no_step_failed(mark);
    every.stop().await;
    server.stop().await;
}

/// The stored span the next rebuild step starts with, in seconds.
async fn stored_span(server: &Server) -> Option<i32> {
    server
        .client()
        .await
        .query_one(
            "SELECT rebuild_span_seconds FROM test_results.classifier_state",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn a_step_too_large_for_the_cap_is_retried_shorter_and_the_next_tick_starts_there() {
    let _serial = SERIAL.lock().await;
    let mark = log_mark();
    // Setup: four days of a main run every 15 minutes over 400 tests,
    // classified up to now by a release that kept no `main_through`, so
    // passes wait for a rebuild under the every-workflow rule. Six hours of
    // it is about 10,000 results.
    let Some(server) = start(&[("ASTER_SRV_RETENTION_DAYS", SHORT_RETENTION)]).await else {
        return;
    };
    let now = clock();
    seed_history(
        &server,
        now - Duration::minutes(20),
        &History {
            days: 4,
            minutes: 15,
            tests: 400,
            prs: 0,
        },
    )
    .await;
    let history = short_history_day(now);
    server
        .client()
        .await
        .execute(
            "INSERT INTO test_results.classifier_state (id, watermark) VALUES (1, $1)",
            &[&now],
        )
        .await
        .unwrap();

    // Fault: with almost no sort memory, a six-hour step's sorts need more
    // temporary file than the cap allows (about 900 kB); three hours' fit.
    let client = server.client().await;
    let database: String = client
        .query_one("SELECT current_database()", &[])
        .await
        .unwrap()
        .get(0);
    client
        .batch_execute(&format!(
            "ALTER DATABASE {database} SET temp_file_limit = '700kB'; \
             ALTER DATABASE {database} SET work_mem = '64kB'"
        ))
        .await
        .unwrap();

    // Action: a tick. Its first step fails at six hours and is retried at
    // three, which the rest of the rebuild keeps.
    let first = server.second(&[]).await;
    tick(&first, now).await;
    let shorter = logged_since(mark, "test_main rebuild step failed; trying a shorter span");
    assert_eq!(shorter.len(), 1, "{shorter:?}");
    assert!(shorter[0].contains("\"minutes\":180"), "{shorter:?}");
    assert_eq!(stored_span(&first).await, Some(3 * 60 * 60));
    assert_eq!(
        rebuild_state(&first).await.1,
        Some(now - OVERLAP),
        "rebuilt and swapped in"
    );
    assert_eq!(
        carried(&first).await,
        expected(&first, history, now - OVERLAP, &main_rule(None)).await
    );
    first.stop().await;

    // Action: `main_through` is lost again, and another replica rebuilds.
    server
        .client()
        .await
        .batch_execute("UPDATE test_results.classifier_state SET main_through = NULL")
        .await
        .unwrap();
    let mark = log_mark();
    let second = server.second(&[]).await;
    tick(&second, now).await;

    // Outcome: it starts at the span that worked, so no step fails, and the
    // span lengthens by a quarter after a tick of steps that all worked.
    no_step_failed(mark);
    assert_eq!(rebuild_state(&second).await.1, Some(now - OVERLAP));
    assert_eq!(stored_span(&second).await, Some(3 * 60 * 60 * 5 / 4));
    second.stop().await;
    server.stop().await;
}
