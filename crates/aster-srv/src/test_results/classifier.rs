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
//! Main sequence: a test's results in runs on the default branch
//! (`ASTER_SRV_DEFAULT_BRANCH`). With `ASTER_SRV_REFERENCE_WORKFLOWS` set,
//! only runs of those workflows (the run's `workflow`, compared exactly)
//! are in it, so a nightly suite on the default branch neither breaks a
//! test nor clears one. Rule 3, broken and broken on main read only the
//! main sequence. Trust (which events quarantine may act on) does not
//! depend on the workflow: every default-branch run with a known source is
//! trusted.
//!
//! Likely flake events (`kind = 'likely'`), only when the run is in the
//! main sequence:
//!
//! 3. `next_main_run`: the final outcome is a pass and the test's previous
//!    main-sequence run failed, whatever the input hash, unless that failure
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
//! Broken: the test's last `ASTER_SRV_BROKEN_RUNS` (at least 3) main-sequence
//! runs all failed with the same, non-null `failure_hash`. `broken_since_sha`
//! is the sha of the first run of that whole streak. The next pass that
//! leaves it healthy clears it. Broken wins over flaky.
//!
//! Broken on main: the same streak at the shorter
//! `ASTER_SRV_MAIN_BROKEN_RUNS` (at least 1, default 2) is kept as
//! `main_broken_since_sha`, `main_broken_failure_hash` and `main_broken_at`
//! on `tests`, independent of `state`. A pass or a different failure in the
//! main sequence clears it in the same pass. Entering and leaving are logged
//! to `quarantine_log` (`main_broken`, `main_fixed`) so the quarantine list's
//! version moves with its `main_broken` array.
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
//! main-sequence result and failure streak up to the window's start
//! (`classifier_state.main_through`), followed by the window's own results;
//! a pass then moves results older than its overlap into it. Quarantine
//! release counts are kept the same way (`quarantine.passes`, recounted from
//! the results for a window that starts anywhere else). Rule 2 compares runs
//! of the same target address, and a failure streak longer than the
//! retention period keeps counting past it.
//!
//! # Rebuilding `test_main`
//!
//! `test_main` is rebuilt from the results when it was built under other
//! reference workflows than the configured ones
//! (`classifier_state.main_workflows`), when a release that knows nothing
//! of them moved it while some were set (`main_workflows_through`), and
//! when it does not end where the next window starts (`main_through`, for
//! example after an upgrade from a release that did not keep it).
//!
//! A rebuild reads the whole retention period, more than one statement can
//! sort: in v0.37.0 it was one statement in the pass, and 30 days of
//! main-sequence results did not fit in production's 5 GB of temporary
//! files, so every pass failed and rolled back. It is built beside `test_main`, in
//! `test_main_rebuild`, by steps of [`REBUILD_SPAN`] of results: each step
//! is its own transaction under the classifier's lock, carries the rows
//! built so far forward the way a pass carries `test_main`, and records how
//! far it got (`classifier_state.rebuild_through`, under the rule it builds
//! for, `rebuild_workflows`). Steps run after a tick's passes, for at most
//! `ASTER_SRV_REBUILD_BUDGET_SECONDS`, and go on in the next tick. When the
//! rebuild reaches the next window's start, that pass swaps it in and judges
//! every test in it again, so a test listed only under the old rule leaves
//! without a new run. A swap that fails rolls back; the tick's next pass
//! carries the old `test_main` instead, and the next tick swaps again. A
//! step that fails is retried at half the span, down to
//! [`REBUILD_MIN_SPAN`]; the span that worked is kept
//! (`rebuild_span_seconds`) and lengthens again by a quarter each tick
//! whose steps all worked. A step reads only its
//! span's main-sequence runs and their results, so its sorts are bounded
//! by the span, not by the retention period; so is the swap, which copies
//! one row per test, and the pass after it, which judges every test once.
//!
//! Until the swap, passes go on with the `test_main` they have, under the
//! rule it was built with: the old reference workflows keep deciding
//! streaks for a few more passes, and nothing waits on the rebuild. Only a
//! `test_main` that does not end where the window starts cannot carry a
//! pass; those passes wait for the rebuild (it reads only results older than
//! the window, which no upload changes).
//!
//! Replicas: steps and passes take the same lock, and each step reads where
//! the next window starts under it. A rebuild depends only on results older
//! than that, so a pass by another replica, or by a release that knows
//! nothing of rebuilds, only moves its target. A replica configured with
//! other reference workflows starts the rebuild over for its own; one that
//! needs none leaves it.

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

/// The span of results one step of a `test_main` rebuild reads. A step's
/// largest statement wrote about 140 bytes of temporary file per main-sequence
/// result with short ids (`tests/main_rebuild.rs`, `rebuild_scale`); with
/// production's longer test, run and commit ids, say 250. Six hours of a
/// main sequence of 175,000 results an hour is then about 0.3 GB, against
/// Cloud SQL's 5 GB `temp_file_limit`.
const REBUILD_SPAN: chrono::Duration = chrono::Duration::hours(6);

/// The shortest span a failing rebuild step is retried at.
const REBUILD_MIN_SPAN: chrono::Duration = chrono::Duration::minutes(5);

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
    let mut locked = false;
    let mut rebuilt = false;
    // Whether a pass may swap a finished rebuild in. A swap that fails is not
    // tried again this tick: the pass after it carries the old `test_main`,
    // which is still right for its rule, and the next tick swaps.
    let mut swap = true;
    loop {
        let mut swapping = false;
        let outcome = match pass(app, now, swap, &mut swapping).await {
            Err(error) if swapping => {
                crate::log::error(
                    "test_main rebuild swap failed; passes go on under the old rule",
                    json!({"error": format!("{error:#}")}),
                );
                swap = false;
                continue;
            }
            outcome => outcome?,
        };
        let rebuild = match outcome {
            None if !locked => return Ok(None),
            None => return Ok(Some(report)),
            Some(Pass::Classified {
                report: chunk,
                done,
                rebuild,
            }) => {
                locked = true;
                report.results += chunk.results;
                report.events += chunk.events;
                report.incidents += chunk.incidents;
                report.tests_updated += chunk.tests_updated;
                report.quarantine.quarantined += chunk.quarantine.quarantined;
                report.quarantine.released += chunk.quarantine.released;
                report.quarantine.expired += chunk.quarantine.expired;
                report.quarantine.broken += chunk.quarantine.broken;
                report.deliveries += chunk.deliveries;
                if !done {
                    continue;
                }
                rebuild
            }
            Some(Pass::Waiting) => {
                locked = true;
                true
            }
        };
        // Once a tick: the rebuild's steps are bounded by its budget, and a
        // pass that still finds it unfinished waits for the next tick.
        if !rebuild || rebuilt {
            return Ok(Some(report));
        }
        rebuilt = true;
        match rebuild_main(app, now).await {
            // The next pass swaps it in, unless a swap failed this tick.
            Ok(Step::Ready) if swap => continue,
            Ok(_) => return Ok(Some(report)),
            // Passes do not depend on it, so one failing does not stop them.
            Err(error) => {
                crate::log::error(
                    "test_main rebuild failed",
                    json!({"error": format!("{error:#}")}),
                );
                return Ok(Some(report));
            }
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
    /// The workflows whose default-branch runs make the main sequence;
    /// empty for every workflow.
    reference_workflows: Vec<String>,
    broken_runs: u32,
    main_broken_runs: u32,
    quarantine: QuarantineRules,
    webhooks: Vec<webhooks::Endpoint>,
}

impl Window {
    /// Runs received in `(from, to]`, as of `now`, with the main sequence
    /// made of `reference_workflows`' runs (every workflow's when empty).
    fn new(
        app: &App,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        now: DateTime<Utc>,
        reference_workflows: Vec<String>,
    ) -> Self {
        Window {
            from,
            to,
            history_from: now.date_naive()
                - chrono::Days::new(u64::from(app.config.retention_days)),
            on_default: app.config.default_branch.clone(),
            reference_workflows,
            broken_runs: app.config.broken_runs,
            main_broken_runs: app.config.main_broken_runs,
            quarantine: app.config.quarantine,
            webhooks: app.config.webhooks.clone(),
        }
    }

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

    /// Whether the run `r` (an alias of `test_results.runs`) is on the
    /// default branch. An OIDC upload's `ref` and `event_name` claims say
    /// so, not its record: a push, dispatch or schedule of the default
    /// branch's own workflow. A pull request's ref is `refs/pull/...`
    /// whatever branch its record names, and `pull_request_target` runs on
    /// the default branch's ref but for a pull request. A static token's
    /// runs, and runs stored before sources were recorded, go by `branch`.
    fn on_default(&self, r: &str) -> String {
        format!(
            "(CASE WHEN {r}.source = 'oidc' \
                   THEN COALESCE({r}.git_ref = {git_ref} \
                                 AND {r}.event_name IN ('push', 'workflow_dispatch', 'schedule'), \
                                 false) \
                   ELSE COALESCE({branch}, false) END)",
            git_ref = text(&format!("refs/heads/{}", self.on_default)),
            branch = self.branch_is_default(&format!("{r}.branch")),
        )
    }

    /// Whether the run `r` is evidence quarantine may act on: a
    /// default-branch run whose source is known. A pull request job runs
    /// its author's code, so its token proves where the results came from
    /// but not that they are true.
    pub(super) fn trusted(&self, r: &str) -> String {
        format!("({r}.source IS NOT NULL AND {})", self.on_default(r))
    }

    /// Whether the run `r` is in each test's main sequence, the runs that
    /// decide broken streaks, broken on main and likely flakes: a
    /// default-branch run, of a reference workflow when any are set. A run
    /// with no workflow name is in no reference workflow.
    pub(super) fn in_main_sequence(&self, r: &str) -> String {
        if self.reference_workflows.is_empty() {
            return self.on_default(r);
        }
        let names: Vec<String> = self.reference_workflows.iter().map(|w| text(w)).collect();
        format!(
            "({} AND COALESCE({r}.workflow IN ({}), false))",
            self.on_default(r),
            names.join(", ")
        )
    }

    /// The main-sequence rule as `classifier_state.main_workflows` keeps
    /// it: `None` for every default-branch run, else the reference
    /// workflows as a JSON array. `test_main` built under another rule is
    /// rebuilt.
    fn main_workflows(&self) -> Option<String> {
        rule(&self.reference_workflows)
    }

    /// Each run's final result of one test: `filter` picks the test and
    /// bounds the runs. `strict` leaves out `partial` targets. `any_fail`
    /// says whether any attempt of the run failed, so it is also true for a
    /// run whose retry passed.
    pub(super) fn finals(&self, filter: &str, strict: bool) -> String {
        format!(
            "SELECT DISTINCT ON (tr.run_id) tr.run_id, r.sha, r.received_at, \
                    tr.outcome IN ('fail', 'error') AS is_fail, tr.failure_hash, \
                    bool_or(tr.outcome IN ('fail', 'error')) \
                        OVER (PARTITION BY tr.run_id) AS any_fail \
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

/// The main-sequence rule as `classifier_state` keeps it: `None` for every
/// default-branch run, else the reference workflows as a JSON array.
fn rule(reference_workflows: &[String]) -> Option<String> {
    (!reference_workflows.is_empty())
        .then(|| serde_json::to_string(reference_workflows).expect("strings serialize"))
}

/// What `classifier_state` says about `test_main` and its rebuild.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Carried {
    watermark: DateTime<Utc>,
    /// Where `test_main` ends.
    main_through: Option<DateTime<Utc>>,
    /// The rule `test_main` was built under.
    main_workflows: Option<String>,
    /// The `main_through` this release last wrote with `main_workflows`.
    main_workflows_through: Option<DateTime<Utc>>,
    /// The rule `test_main_rebuild` is built under.
    rebuild_workflows: Option<String>,
    /// Where `test_main_rebuild` ends; `None` when no rebuild is under way.
    rebuild_through: Option<DateTime<Utc>>,
}

const CARRIED: &str = "SELECT watermark, main_through, main_workflows, main_workflows_through, \
                              rebuild_workflows, rebuild_through \
                       FROM test_results.classifier_state";

impl Carried {
    fn read(row: &tokio_postgres::Row) -> Self {
        Carried {
            watermark: row.get(0),
            main_through: row.get(1),
            main_workflows: row.get(2),
            main_workflows_through: row.get(3),
            rebuild_workflows: row.get(4),
            rebuild_through: row.get(5),
        }
    }

    /// A release that knows nothing of reference workflows (a replica not
    /// yet replaced in a rolling deploy) moved `main_through` while some
    /// were set: it added every workflow's results to the tests it saw.
    fn tainted(&self) -> bool {
        self.main_workflows.is_some() && self.main_workflows_through != self.main_through
    }

    /// Whether `test_main` should be rebuilt for a window starting at
    /// `from` under the rule `configured`.
    fn stale(&self, from: DateTime<Utc>, configured: &Option<String>) -> bool {
        self.main_through != Some(from)
            || self.main_workflows != *configured
            || self.tainted()
            || self.rule().is_none()
    }

    /// The reference workflows `test_main` was built with, or `None` when
    /// what is stored is not a list of names.
    fn rule(&self) -> Option<Vec<String>> {
        match &self.main_workflows {
            None => Some(Vec::new()),
            Some(rule) => serde_json::from_str(rule).ok(),
        }
    }

    /// What a pass over a window starting at `from` does with `test_main`;
    /// `swap` says whether it may swap a finished rebuild in.
    fn plan(&self, from: DateTime<Utc>, configured: &Option<String>, swap: bool) -> Plan {
        if !self.stale(from, configured) {
            return Plan::Carry {
                rule: self.rule().expect("stale when not a list"),
                rebuild: false,
            };
        }
        if swap && self.rebuild_through == Some(from) && self.rebuild_workflows == *configured {
            return Plan::Swap;
        }
        // Under the rule it was built with, which may not be the configured
        // one: `test_main` stays valid for that rule until the swap.
        if self.main_through == Some(from) {
            if let Some(rule) = self.rule() {
                return Plan::Carry {
                    rule,
                    rebuild: true,
                };
            }
        }
        Plan::Wait
    }
}

/// What a pass does with `test_main`.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// No pass has run: the window starts before the oldest run, so
    /// `test_main` starts empty under the configured rule.
    Empty,
    /// `test_main` ends where the window starts: carry it under `rule`, the
    /// reference workflows it was built with. `rebuild` says it is due to be
    /// replaced.
    Carry { rule: Vec<String>, rebuild: bool },
    /// The rebuild ends where the window starts: swap it in and judge every
    /// test in it again.
    Swap,
    /// Nothing ends where the window starts: classify nothing until the
    /// rebuild does.
    Wait,
}

/// What one transaction did.
enum Pass {
    /// It classified a window: what it did, whether it reached `now`, and
    /// whether `test_main` is due a rebuild.
    Classified {
        report: Report,
        done: bool,
        rebuild: bool,
    },
    /// It found nothing to carry the window and classified nothing.
    Waiting,
}

/// One transaction: classify the next chunk of runs, or `None` when the
/// lock is held. `swap` says whether it may swap a finished rebuild in;
/// `swapping` is set when it tries to, so a failure can be told apart.
async fn pass(
    app: &App,
    now: DateTime<Utc>,
    swap: bool,
    swapping: &mut bool,
) -> Result<Option<Pass>> {
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
    let mut window = Window::new(
        app,
        watermark - OVERLAP,
        to,
        now,
        app.config.reference_workflows.clone(),
    );

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
    let carried = transaction
        .query_opt(CARRIED, &[])
        .await?
        .map(|row| Carried::read(&row));
    let committed = carried.as_ref().map(|carried| carried.watermark);
    if let Some(committed) = committed.filter(|committed| *committed > window.to) {
        transaction.rollback().await?;
        return Ok(Some(Pass::Classified {
            report: Report::default(),
            done: committed >= now,
            rebuild: false,
        }));
    }
    // Another replica moved the watermark between the read above and the
    // lock: this window no longer starts where `test_main` ends. Read again.
    if committed.is_some_and(|committed| committed != watermark) {
        transaction.rollback().await?;
        return Ok(Some(Pass::Classified {
            report: Report::default(),
            done: false,
            rebuild: false,
        }));
    }
    let configured = window.main_workflows();
    let plan = match &carried {
        Some(carried) => carried.plan(window.from, &configured, swap),
        None => Plan::Empty,
    };
    let mut swapped = None;
    let (rule_changed, rebuild) = match &plan {
        Plan::Empty => {
            transaction
                .batch_execute("DELETE FROM test_results.test_main")
                .await?;
            (false, false)
        }
        Plan::Carry { rule, rebuild } => {
            window.reference_workflows = rule.clone();
            (false, *rebuild)
        }
        // After the rule changed, a healthy test with no new run may now be
        // broken or broken on main (its old main sequence passed, its new
        // one fails): every test in the rebuilt `test_main` is judged again.
        Plan::Swap => {
            *swapping = true;
            transaction
                .batch_execute("DELETE FROM test_results.test_main")
                .await?;
            let tests = transaction
                .execute(
                    &format!(
                        "INSERT INTO test_results.test_main ({MAIN_COLUMNS}) \
                         SELECT {MAIN_COLUMNS} FROM test_results.test_main_rebuild"
                    ),
                    &[],
                )
                .await?;
            transaction
                .batch_execute("DELETE FROM test_results.test_main_rebuild")
                .await?;
            swapped = Some((tests, std::time::Instant::now()));
            (true, false)
        }
        Plan::Wait => {
            transaction.rollback().await?;
            return Ok(Some(Pass::Waiting));
        }
    };
    let report = classify(&transaction, &window, rule_changed).await?;
    // A taint stays until a rebuild replaces it: `main_workflows_through`
    // is moved with `main_through` only when `test_main` is clean.
    let tainted =
        matches!(plan, Plan::Carry { .. }) && carried.as_ref().is_some_and(Carried::tainted);
    let main_workflows_through = if tainted {
        carried.as_ref().and_then(|c| c.main_workflows_through)
    } else {
        Some(window.through())
    };
    // A rebuild is dropped only once swapped in. One this pass does not need
    // (the rule was changed back, or it is another replica's) is left: it is
    // still right for its rule up to where it got, and a replica that wants
    // that rule goes on from there.
    transaction
        .execute(
            "INSERT INTO test_results.classifier_state \
                 (id, watermark, main_through, main_workflows, main_workflows_through) \
             VALUES (1, $1, $2, $3, $4) \
             ON CONFLICT (id) DO UPDATE SET \
                 watermark = GREATEST(test_results.classifier_state.watermark, EXCLUDED.watermark), \
                 main_through = EXCLUDED.main_through, \
                 main_workflows = EXCLUDED.main_workflows, \
                 main_workflows_through = EXCLUDED.main_workflows_through, \
                 rebuild_workflows = CASE WHEN NOT $5 \
                     THEN test_results.classifier_state.rebuild_workflows END, \
                 rebuild_through = CASE WHEN NOT $5 \
                     THEN test_results.classifier_state.rebuild_through END, \
                 updated_at = now()",
            &[
                &window.to,
                &window.through(),
                &window.main_workflows(),
                &main_workflows_through,
                &(plan == Plan::Swap),
            ],
        )
        .await?;
    transaction.commit().await?;
    if let Some((tests, started)) = swapped {
        crate::log::info(
            "test_main rebuild swapped in",
            json!({
                "tests": tests,
                "tests_updated": report.tests_updated,
                "seconds": started.elapsed().as_secs_f64(),
                "rule": window.main_workflows(),
            }),
        );
    }
    Ok(Some(Pass::Classified {
        report,
        done,
        rebuild,
    }))
}

/// What a rebuild step found.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Nothing to do now: no rebuild is due, or another replica holds the
    /// lock.
    Idle,
    /// It built up to `through`, short of `target`, where the next window
    /// starts.
    Advanced {
        through: DateTime<Utc>,
        target: DateTime<Utc>,
    },
    /// The rebuild ends where the next window starts.
    Ready,
}

/// Rebuild steps for up to the configured budget
/// (`ASTER_SRV_REBUILD_BUDGET_SECONDS`).
///
/// Steps start at the span the last one that worked read
/// (`classifier_state.rebuild_span_seconds`, shared by replicas and kept
/// across restarts). A step that fails is retried at half its span, however
/// long it took, down to [`REBUILD_MIN_SPAN`]; the shorter span is stored at
/// once, so the next tick does not start with a step that fails. A tick whose
/// steps all worked lengthens the span by a quarter, up to [`REBUILD_SPAN`].
async fn rebuild_main(app: &App, now: DateTime<Utc>) -> Result<Step> {
    let started = std::time::Instant::now();
    let mut span = rebuild_span(app).await?;
    let mut failed = false;
    let mut steps = 0;
    loop {
        match rebuild_step(app, now, span).await {
            Ok(Step::Advanced { through, target }) => {
                steps += 1;
                if started.elapsed() >= app.config.rebuild_budget {
                    if !failed {
                        store_rebuild_span(app, (span * 5 / 4).min(REBUILD_SPAN)).await?;
                    }
                    crate::log::info(
                        "test_main rebuild continues next pass",
                        json!({
                            "steps": steps,
                            "through": through.to_rfc3339(),
                            "target": target.to_rfc3339(),
                            "seconds": started.elapsed().as_secs(),
                        }),
                    );
                    return Ok(Step::Advanced { through, target });
                }
            }
            Ok(Step::Ready) => {
                if !failed {
                    store_rebuild_span(app, (span * 5 / 4).min(REBUILD_SPAN)).await?;
                }
                crate::log::info(
                    "test_main rebuild caught up",
                    json!({"steps": steps + 1, "seconds": started.elapsed().as_secs()}),
                );
                return Ok(Step::Ready);
            }
            Ok(Step::Idle) => return Ok(Step::Idle),
            Err(error) if span > REBUILD_MIN_SPAN => {
                failed = true;
                span = (span / 2).max(REBUILD_MIN_SPAN);
                store_rebuild_span(app, span).await?;
                crate::log::warn(
                    "test_main rebuild step failed; trying a shorter span",
                    json!({"error": format!("{error:#}"), "minutes": span.num_minutes()}),
                );
            }
            Err(error) => return Err(error),
        }
    }
}

/// The span the next rebuild step reads: the stored one, within
/// [`REBUILD_MIN_SPAN`] and [`REBUILD_SPAN`].
async fn rebuild_span(app: &App) -> Result<chrono::Duration> {
    let stored: Option<i32> = app
        .pool
        .get()
        .await?
        .query_opt(
            "SELECT rebuild_span_seconds FROM test_results.classifier_state",
            &[],
        )
        .await?
        .and_then(|row| row.get(0));
    Ok(stored.map_or(REBUILD_SPAN, |seconds| {
        chrono::Duration::seconds(i64::from(seconds)).clamp(REBUILD_MIN_SPAN, REBUILD_SPAN)
    }))
}

/// Keep `span` for the next rebuild step. Outside the classifier's lock: it
/// is a hint, and two replicas storing one each leave a span that worked.
async fn store_rebuild_span(app: &App, span: chrono::Duration) -> Result<()> {
    let seconds = i32::try_from(span.num_seconds()).expect("a span of hours");
    app.pool
        .get()
        .await?
        .execute(
            "UPDATE test_results.classifier_state SET rebuild_span_seconds = $1",
            &[&seconds],
        )
        .await?;
    Ok(())
}

/// One transaction: add up to `span` of results to `test_main_rebuild`.
async fn rebuild_step(app: &App, now: DateTime<Utc>, span: chrono::Duration) -> Result<Step> {
    let mut client = app.pool.get().await?;
    let transaction = client.transaction().await?;
    let locked: bool = transaction
        .query_one("SELECT pg_try_advisory_xact_lock($1)", &[&CLASSIFIER_LOCK])
        .await?
        .get(0);
    if !locked {
        return Ok(Step::Idle);
    }
    let Some(carried) = transaction
        .query_opt(CARRIED, &[])
        .await?
        .map(|row| Carried::read(&row))
    else {
        return Ok(Step::Idle);
    };
    let configured = app.config.reference_workflows.clone();
    let rule = rule(&configured);
    // The next pass's window starts here.
    let target = carried.watermark - OVERLAP;
    if !carried.stale(target, &rule) {
        return Ok(Step::Idle);
    }
    let from = match carried.rebuild_through {
        Some(through) if carried.rebuild_workflows == rule && through <= target => through,
        // Start over: from the first day kept, or nothing when the next
        // window starts before it.
        _ => {
            crate::log::info(
                "test_main rebuild starts",
                json!({
                    "rule": rule,
                    "replaces_rule": carried.rebuild_through.map(|_| &carried.rebuild_workflows),
                    "target": target.to_rfc3339(),
                }),
            );
            transaction
                .batch_execute("DELETE FROM test_results.test_main_rebuild")
                .await?;
            let history =
                now.date_naive() - chrono::Days::new(u64::from(app.config.retention_days));
            let first = history.and_hms_opt(0, 0, 0).expect("midnight").and_utc()
                - chrono::Duration::microseconds(1);
            first.min(target)
        }
    };
    // A span with no runs is skipped: the step starts at the next run.
    let next: Option<DateTime<Utc>> = transaction
        .query_one(
            "SELECT min(received_at) FROM test_results.runs \
             WHERE received_at > $1 AND received_at <= $2",
            &[&from, &target],
        )
        .await?
        .get(0);
    let to = match next {
        Some(next) => {
            let from = from.max(next - chrono::Duration::microseconds(1));
            let to = (from + span).min(target);
            rebuild_chunk(&transaction, &Window::new(app, from, to, now, configured)).await?;
            to
        }
        None => target,
    };
    transaction
        .execute(
            "UPDATE test_results.classifier_state \
             SET rebuild_workflows = $1, rebuild_through = $2",
            &[&rule, &to],
        )
        .await?;
    transaction.commit().await?;
    Ok(if to >= target {
        Step::Ready
    } else {
        Step::Advanced {
            through: to,
            target,
        }
    })
}

/// Carry `test_main_rebuild` over the main-sequence results received in
/// `window`: each test's last one and its streak, the way a pass carries
/// `test_main`.
async fn rebuild_chunk(
    transaction: &tokio_postgres::Transaction<'_>,
    window: &Window,
) -> Result<()> {
    // The window's main runs, then each run's results and target runs by
    // `run_id`: a step reads only main-sequence rows, not whole days of
    // every run (a span shorter than a day would read each day again). The
    // `OFFSET 0` keeps each lookup per run; a plain join was planned as a
    // scan of the day, or as one index scan of the run per target. Tables,
    // not CTEs, so the planner knows how many rows each holds.
    let days = format!(
        "BETWEEN {} AND {}",
        day(window.from.date_naive()),
        day(window.to.date_naive())
    );
    transaction
        .batch_execute(&format!(
            "CREATE TEMP TABLE cls_rebuild_runs ON COMMIT DROP AS \
             SELECT r.run_id, r.received_at, r.sha FROM test_results.runs r \
             WHERE r.received_at > {from} AND r.received_at <= {to} AND {main}; \
             ANALYZE cls_rebuild_runs; \
             CREATE TEMP TABLE cls_rebuild_results ON COMMIT DROP AS \
             SELECT tr.* FROM cls_rebuild_runs r CROSS JOIN LATERAL ( \
                 SELECT tr.repo, tr.test_id, tr.run_id, tr.target, tr.received_on, \
                        tr.attempt, tr.outcome, tr.failure_hash \
                 FROM test_results.test_results tr \
                 WHERE tr.run_id = r.run_id AND tr.received_on {days} \
                   AND tr.outcome <> 'skip' OFFSET 0) tr; \
             ANALYZE cls_rebuild_results; \
             CREATE TEMP TABLE cls_rebuild_targets ON COMMIT DROP AS \
             SELECT t.* FROM cls_rebuild_runs r CROSS JOIN LATERAL ( \
                 SELECT t.run_id, t.address, t.received_on \
                 FROM test_results.target_runs t \
                 WHERE t.run_id = r.run_id AND t.received_on {days} \
                   AND NOT t.cached AND t.confidence IS DISTINCT FROM 'partial' OFFSET 0) t; \
             ANALYZE cls_rebuild_targets; \
             CREATE TEMP TABLE cls_rebuild ON COMMIT DROP AS \
             SELECT DISTINCT ON (tr.run_id, tr.test_id) \
                    tr.repo, tr.test_id, tr.run_id, r.received_at, tr.received_on, r.sha, \
                    tr.outcome IN ('fail', 'error') AS is_fail, tr.failure_hash, \
                    false AS is_seed, 0 AS seed_streak, NULL::text AS seed_since \
             FROM cls_rebuild_results tr \
             JOIN cls_rebuild_runs r ON r.run_id = tr.run_id \
             JOIN cls_rebuild_targets t \
                  ON t.run_id = tr.run_id AND t.address = tr.target \
                 AND t.received_on = tr.received_on \
             ORDER BY tr.run_id, tr.test_id, tr.attempt DESC;",
            from = at(window.from),
            to = at(window.to),
            main = window.in_main_sequence("r"),
        ))
        .await?;
    transaction.batch_execute("ANALYZE cls_rebuild").await?;
    transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_main_rebuild ({MAIN_COLUMNS}) \
                 SELECT repo, test_id, run_id, received_at, received_on, sha, is_fail, \
                        failure_hash, streak, since \
                 FROM {} s WHERE is_last AND NOT is_seed \
                 ON CONFLICT (repo, test_id) DO UPDATE SET {MAIN_UPDATE}",
                // A seed older than the retention period no longer counts,
                // as in a pass.
                streaks(&format!(
                    "SELECT m.repo, m.test_id, m.run_id, m.received_at, m.received_on, m.sha, \
                            m.is_fail, m.failure_hash, true AS is_seed, \
                            m.streak AS seed_streak, m.streak_since_sha AS seed_since \
                     FROM test_results.test_main_rebuild m \
                     WHERE m.received_on >= {history} \
                       AND (m.repo, m.test_id) IN (SELECT repo, test_id FROM cls_rebuild) \
                     UNION ALL \
                     SELECT * FROM cls_rebuild",
                    history = day(window.history_from),
                ))
            ),
            &[],
        )
        .await?;
    Ok(())
}

const EVENT_COLUMNS: &str = "observed_on, repo, test_id, kind, reason, run_id, prev_run_id, \
                             target, input_hash, observed_at, trusted";
const EVENT_KEY: &str = "(repo, test_id, kind, run_id, observed_on)";

async fn classify(
    transaction: &tokio_postgres::Transaction<'_>,
    window: &Window,
    rule_changed: bool,
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
                            {in_main} AS in_main, {trusted} AS trusted, \
                            t.input_hash, t.confidence, \
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
                        failure_hash, received_on, received_at, sha, in_main, trusted, \
                        input_hash, confidence IS NOT DISTINCT FROM 'partial' AS partial, \
                        any_fail AND outcome = 'pass' AS retried \
                 FROM attempts WHERE rn = 1",
                in_main = window.in_main_sequence("r"),
                trusted = window.trusted("r"),
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
                        NULL, w.target, w.input_hash, w.received_at, w.trusted \
                 FROM cls_win w WHERE w.retried \
                   AND NOT EXISTS (SELECT 1 FROM test_results.target_incidents i \
                                   WHERE i.run_id = w.run_id AND i.target = w.target) \
                 ON CONFLICT {EVENT_KEY} DO NOTHING"
            ),
            &[],
        )
        .await?;

    // `test_main` holds each test's main sequence up to `window.from` (see
    // `pass`). After it was rebuilt, every test in it is re-derived, not only
    // those already unhealthy or in the window.
    let rejudged = if rule_changed {
        "UNION SELECT repo, test_id FROM test_results.test_main"
    } else {
        ""
    };

    // Rule 2: the same input hash gave a different outcome than the nearest
    // earlier run of that hash. Runs of one target with one hash are few
    // (`target_runs_hash`), so this starts from them, the window's own
    // included, not from each test's history. A table, not a CTE, so the
    // planner knows how few.
    transaction
        .execute(
            &format!(
                "CREATE TEMP TABLE cls_hash ON COMMIT DROP AS \
                 SELECT t.repo, t.address, t.input_hash, t.run_id, t.received_on, r.received_at, \
                        {trusted} AS trusted \
                 FROM (SELECT repo, target, input_hash, max(received_on) AS until \
                       FROM cls_win WHERE input_hash IS NOT NULL \
                       GROUP BY repo, target, input_hash) k \
                 JOIN test_results.target_runs t \
                      ON t.repo = k.repo AND t.address = k.target \
                     AND t.input_hash = k.input_hash \
                     AND t.received_on BETWEEN {history} AND k.until \
                 JOIN test_results.runs r ON r.run_id = t.run_id \
                 WHERE NOT t.cached",
                trusted = window.trusted("r"),
            ),
            &[],
        )
        .await?;
    transaction.batch_execute("ANALYZE cls_hash").await?;
    // The event is the failed run's, whichever of the two runs that was, so
    // a pass, a failure and a pass with one hash make one event: the second
    // comparison finds the first one's key. A trusted run is also compared
    // with the nearest earlier trusted run of its hash, so pull request runs
    // in between cannot hide a flake main saw for itself; the event is
    // trusted when both its runs are, and a trusted comparison found later
    // upgrades one found first.
    events += transaction
        .execute(
            &format!(
                "WITH earlier AS MATERIALIZED ( \
                     SELECT DISTINCT ON (tr.run_id, tr.test_id) \
                            tr.repo, tr.test_id, h.address, h.input_hash, tr.run_id, \
                            h.received_at, h.received_on, h.trusted, \
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
                            p.received_at AS prev_received_at, p.received_on AS prev_received_on, \
                            p.trusted AS prev_trusted \
                     FROM cls_win w \
                     JOIN earlier p \
                          ON p.repo = w.repo AND p.test_id = w.test_id \
                         AND p.address = w.target AND p.input_hash = w.input_hash \
                         AND (p.received_at, p.run_id) < (w.received_at, w.run_id) \
                     ORDER BY w.run_id, w.test_id, p.received_at DESC, p.run_id DESC), \
                 nearest_trusted AS ( \
                     SELECT DISTINCT ON (w.run_id, w.test_id) w.*, \
                            p.run_id AS prev_run_id, p.is_fail AS prev_is_fail, \
                            p.received_at AS prev_received_at, p.received_on AS prev_received_on, \
                            p.trusted AS prev_trusted \
                     FROM cls_win w \
                     JOIN earlier p \
                          ON p.repo = w.repo AND p.test_id = w.test_id \
                         AND p.address = w.target AND p.input_hash = w.input_hash \
                         AND (p.received_at, p.run_id) < (w.received_at, w.run_id) \
                     WHERE w.trusted AND p.trusted \
                     ORDER BY w.run_id, w.test_id, p.received_at DESC, p.run_id DESC), \
                 flips AS ( \
                     SELECT repo, test_id, target, input_hash, \
                            CASE WHEN is_fail THEN run_id ELSE prev_run_id END AS failed_run, \
                            CASE WHEN is_fail THEN prev_run_id ELSE run_id END AS passed_run, \
                            CASE WHEN is_fail THEN received_on ELSE prev_received_on END AS failed_on, \
                            CASE WHEN is_fail THEN received_at ELSE prev_received_at END AS failed_at, \
                            trusted AND prev_trusted AS trusted \
                     FROM (SELECT * FROM nearest UNION ALL SELECT * FROM nearest_trusted) n \
                     WHERE prev_is_fail <> is_fail), \
                 kept AS ( \
                     SELECT DISTINCT ON (repo, test_id, failed_run, failed_on) * FROM flips f \
                     WHERE NOT EXISTS (SELECT 1 FROM test_results.target_incidents i \
                                       WHERE i.run_id = f.failed_run AND i.target = f.target) \
                     ORDER BY repo, test_id, failed_run, failed_on, trusted DESC, passed_run) \
                 INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT f.failed_on, f.repo, f.test_id, 'definite', 'input_hash', f.failed_run, \
                        f.passed_run, f.target, f.input_hash, f.failed_at, f.trusted \
                 FROM kept f \
                 ON CONFLICT {EVENT_KEY} DO UPDATE \
                     SET trusted = true, prev_run_id = EXCLUDED.prev_run_id \
                     WHERE EXCLUDED.trusted AND NOT test_results.test_flake_events.trusted"
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
    // result before it and the failure streak it ends. The seeds are picked
    // with `IN` over one key set, not `EXISTS (cls_win) OR EXISTS (tests)`:
    // under `OR` Postgres cannot turn the correlated `EXISTS` into a join, so
    // it scanned `cls_win` once per `test_main` row (30k rows times a 234k-row
    // hour of results did not finish in five minutes in production).
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
                       AND (m.repo, m.test_id) IN ( \
                           SELECT repo, test_id FROM cls_win \
                           UNION \
                           SELECT repo, test_id FROM test_results.tests \
                           WHERE state <> 'healthy' OR flake_score > 0 \
                              OR main_broken_failure_hash IS NOT NULL \
                           {rejudged}) \
                     UNION ALL \
                     SELECT repo, test_id, run_id, received_at, received_on, sha, is_fail, \
                            failure_hash, false, 0, NULL::text \
                     FROM cls_win WHERE in_main AND NOT partial"
                ))
            ),
            &[],
        )
        .await?;
    // Indexed for the per-test joins below. The state update's estimate after
    // its `IS DISTINCT FROM` filter is about one row, so without an index it
    // took a nested loop that scanned `cls_main` once per test (200s for
    // 20,000 tests in `classifier_bench`).
    transaction
        .batch_execute("CREATE INDEX ON cls_main (repo, test_id); ANALYZE cls_main")
        .await?;

    // Rule 3: a main-sequence pass right after a main-sequence failure
    // that did not end a broken streak. The event is the failed run's,
    // unless that run already has a definite one.
    events += transaction
        .execute(
            &format!(
                "INSERT INTO test_results.test_flake_events ({EVENT_COLUMNS}) \
                 SELECT m.prev_received_on, w.repo, w.test_id, 'likely', 'next_main_run', \
                        m.prev_run_id, w.run_id, f.target, t.input_hash, m.prev_received_at, false \
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
                 WHERE w.in_main AND NOT w.partial AND NOT w.is_fail \
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
    // that is not healthy, whose score ages or whose streak may end. Only
    // the tests whose state changes are kept.
    transaction
        .execute(
            &format!(
                "CREATE TEMP TABLE cls_state ON COMMIT DROP AS \
                 WITH affected AS ( \
                     SELECT repo, test_id FROM cls_win \
                     UNION \
                     SELECT repo, test_id FROM test_results.tests \
                     WHERE state <> 'healthy' OR flake_score > 0 \
                        OR main_broken_failure_hash IS NOT NULL \
                     {rejudged}), \
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
                            COALESCE(k.length, 0) >= {mk} AS main_broken, \
                            k.since_sha, k.failure_hash \
                     FROM scores s LEFT JOIN streaks k USING (repo, test_id)), \
                 next AS ( \
                     SELECT repo, test_id, \
                            CASE WHEN broken THEN 'broken' \
                                 WHEN score > 0 THEN 'flaky' ELSE 'healthy' END AS state, \
                            score AS flake_score, last_flake_at, \
                            CASE WHEN broken THEN since_sha END AS broken_since_sha, \
                            CASE WHEN broken THEN failure_hash END AS broken_failure_hash, \
                            CASE WHEN main_broken THEN since_sha END AS main_broken_since_sha, \
                            CASE WHEN main_broken THEN failure_hash END AS main_broken_failure_hash \
                     FROM derived) \
                 SELECT n.repo, n.test_id, n.state, n.flake_score, n.last_flake_at, \
                        n.broken_since_sha, n.broken_failure_hash, \
                        n.main_broken_since_sha, n.main_broken_failure_hash, \
                        CASE WHEN n.main_broken_failure_hash IS NULL THEN NULL \
                             WHEN (o.main_broken_since_sha, o.main_broken_failure_hash) \
                                  IS NOT DISTINCT FROM \
                                  (n.main_broken_since_sha, n.main_broken_failure_hash) \
                             THEN o.main_broken_at \
                             ELSE {to} END AS main_broken_at, \
                        o.state AS old_state, \
                        o.main_broken_failure_hash AS old_main_broken_failure_hash \
                 FROM next n JOIN test_results.tests o USING (repo, test_id) \
                 WHERE (o.state, o.flake_score, o.last_flake_at, \
                        o.broken_since_sha, o.broken_failure_hash, \
                        o.main_broken_since_sha, o.main_broken_failure_hash) \
                       IS DISTINCT FROM \
                       (n.state, n.flake_score, n.last_flake_at, \
                        n.broken_since_sha, n.broken_failure_hash, \
                        n.main_broken_since_sha, n.main_broken_failure_hash)",
                to = at(window.to),
                k = window.broken_runs,
                mk = window.main_broken_runs,
            ),
            &[],
        )
        .await?;
    // Uploads upsert `tests` too. Locking the changed rows in (repo, test_id)
    // "C" order first, the order uploads write in, means an upload and this
    // pass wait for each other instead of deadlocking; the `UPDATE` alone
    // locks in whatever order its join produces.
    transaction
        .execute(
            "SELECT 1 FROM test_results.tests t JOIN cls_state s USING (repo, test_id) \
             ORDER BY t.repo COLLATE \"C\", t.test_id COLLATE \"C\" \
             FOR NO KEY UPDATE OF t",
            &[],
        )
        .await?;
    let tests_updated = transaction
        .query(
            "UPDATE test_results.tests t SET \
                 state = s.state, flake_score = s.flake_score, last_flake_at = s.last_flake_at, \
                 broken_since_sha = s.broken_since_sha, \
                 broken_failure_hash = s.broken_failure_hash, \
                 main_broken_since_sha = s.main_broken_since_sha, \
                 main_broken_failure_hash = s.main_broken_failure_hash, \
                 main_broken_at = s.main_broken_at \
             FROM cls_state s \
             WHERE t.repo = s.repo AND t.test_id = s.test_id \
             RETURNING t.repo, t.test_id, t.target, t.file, t.suite, t.name, t.state, \
                       s.old_state, t.broken_since_sha, t.broken_failure_hash",
            &[],
        )
        .await?;
    // A test entering, leaving or changing its main failure changes the
    // quarantine list, so it is a log row: the newest row is the list's
    // version.
    transaction
        .execute(
            &format!(
                "INSERT INTO test_results.quarantine_log (repo, test_id, action, at, detail) \
                 SELECT repo, test_id, \
                        CASE WHEN main_broken_failure_hash IS NULL \
                             THEN 'main_fixed' ELSE 'main_broken' END, \
                        {to}, \
                        jsonb_build_object('since_sha', main_broken_since_sha, \
                                           'failure_hash', COALESCE(main_broken_failure_hash, \
                                                                    old_main_broken_failure_hash)) \
                 FROM cls_state \
                 WHERE main_broken_failure_hash IS DISTINCT FROM old_main_broken_failure_hash \
                    OR (main_broken_failure_hash IS NOT NULL AND main_broken_at = {to}) \
                 ORDER BY repo, test_id",
                to = at(window.to),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn window(reference_workflows: &[&str]) -> Window {
        Window {
            from: Utc::now(),
            to: Utc::now(),
            history_from: Utc::now().date_naive(),
            on_default: "main".into(),
            reference_workflows: reference_workflows.iter().map(|w| w.to_string()).collect(),
            broken_runs: 3,
            main_broken_runs: 2,
            quarantine: QuarantineRules {
                runs: 2,
                release_passes: 20,
                days: 30,
                run_cap: 10,
            },
            webhooks: Vec::new(),
        }
    }

    #[test]
    fn the_main_sequence_is_the_default_branch_unless_reference_workflows_are_set() {
        // Unset: exactly the default-branch rule, and no fingerprint, which
        // is what every release before the setting stored.
        let all = window(&[]);
        assert_eq!(all.in_main_sequence("r"), all.on_default("r"));
        assert_eq!(all.main_workflows(), None);

        // Set: the default-branch rule and the workflow, quoted. Trust is
        // the default-branch rule either way.
        let some = window(&["Nightly", "Stable CI's"]);
        assert_eq!(
            some.in_main_sequence("r"),
            format!(
                "({} AND COALESCE(r.workflow IN ('Nightly', 'Stable CI''s'), false))",
                some.on_default("r")
            )
        );
        assert_eq!(some.trusted("r"), all.trusted("r"));
        assert_eq!(
            some.main_workflows().as_deref(),
            Some(r#"["Nightly","Stable CI's"]"#)
        );
    }

    #[test]
    fn a_pass_carries_test_main_under_its_own_rule_until_a_rebuild_reaches_the_window() {
        let from: DateTime<Utc> = "2026-11-24T10:00:00Z".parse().unwrap();
        let earlier = from - chrono::Duration::hours(1);
        let stable = Some(r#"["Stable CI"]"#.to_string());
        let current = Carried {
            watermark: from + OVERLAP,
            main_through: Some(from),
            main_workflows: None,
            main_workflows_through: Some(from),
            ..Carried::default()
        };
        // Same rule, ending at the window: carried, nothing to rebuild.
        assert_eq!(
            current.plan(from, &None, true),
            Plan::Carry {
                rule: vec![],
                rebuild: false
            }
        );
        // The configured rule changed: carried under the rule it was built
        // with until a rebuild for the new one reaches the window.
        assert_eq!(
            current.plan(from, &stable, true),
            Plan::Carry {
                rule: vec![],
                rebuild: true
            }
        );
        let behind = Carried {
            rebuild_workflows: stable.clone(),
            rebuild_through: Some(earlier),
            ..current.clone()
        };
        assert!(matches!(
            behind.plan(from, &stable, true),
            Plan::Carry { rebuild: true, .. }
        ));
        let caught_up = Carried {
            rebuild_through: Some(from),
            ..behind.clone()
        };
        assert_eq!(caught_up.plan(from, &stable, true), Plan::Swap);
        // After a swap failed, the old `test_main` carries the pass.
        assert!(matches!(
            caught_up.plan(from, &stable, false),
            Plan::Carry { rebuild: true, .. }
        ));
        // A rebuild for another rule is not swapped in.
        assert!(matches!(
            caught_up.plan(from, &Some(r#"["Nightly"]"#.into()), true),
            Plan::Carry { rebuild: true, .. }
        ));

        // An older release moved `main_through` while a rule was set: carried
        // under that rule, and due a rebuild even though the rule is the same.
        let tainted = Carried {
            main_workflows: stable.clone(),
            main_workflows_through: Some(earlier),
            ..current.clone()
        };
        assert!(tainted.tainted());
        assert_eq!(
            tainted.plan(from, &stable, true),
            Plan::Carry {
                rule: vec!["Stable CI".into()],
                rebuild: true
            }
        );

        // `test_main` ends somewhere else, or was never kept: nothing can
        // carry the window until the rebuild reaches it.
        for main_through in [None, Some(earlier)] {
            let lost = Carried {
                main_through,
                ..current.clone()
            };
            assert_eq!(lost.plan(from, &None, true), Plan::Wait);
            let rebuilt = Carried {
                rebuild_workflows: None,
                rebuild_through: Some(from),
                ..lost
            };
            assert_eq!(rebuilt.plan(from, &None, true), Plan::Swap);
            assert_eq!(rebuilt.plan(from, &None, false), Plan::Wait);
        }
        // A stored rule that is not a list of names is rebuilt, not trusted.
        let garbled = Carried {
            main_workflows: Some("Stable CI".into()),
            main_workflows_through: Some(from),
            ..current
        };
        assert!(garbled.stale(from, &Some("Stable CI".into())));
        assert_eq!(
            garbled.plan(from, &Some("Stable CI".into()), true),
            Plan::Wait
        );
    }

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
