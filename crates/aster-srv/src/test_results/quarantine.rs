//! Quarantine: tests proven flaky enough that Aster stops letting them fail
//! the build.
//!
//! The classifier keeps `quarantine` current at the end of each pass, in the
//! pass's transaction and as of its clock, after it has re-derived every
//! test's state:
//!
//! 1. A quarantined test that is now `broken` leaves at once: broken tests
//!    are never quarantined.
//! 2. An entry older than `ASTER_SRV_QUARANTINE_DAYS` expires.
//! 3. A test whose final results since it entered end in
//!    `ASTER_SRV_QUARANTINE_RELEASE_PASSES` consecutive passes is released.
//! 4. A test that is not broken enters when definite flake events from
//!    `ASTER_SRV_QUARANTINE_RUNS` different runs fall in the last 7 days.
//!    Only events after the test last left count, so a released or expired
//!    test is judged afresh.
//!
//! Every change is a row in `quarantine_log`, kept for the retention period.
//! The newest log id of a repository is its quarantine list's version.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::classifier::Window;
use crate::config::QuarantineRules;
use crate::webhooks::Notification;

/// Days of definite events that count toward entering.
const ENTRY_DAYS: i64 = 7;

pub const MIGRATION_SQL: &str = r#"
CREATE TABLE test_results.quarantine (
    repo text NOT NULL,
    test_id text NOT NULL,
    entered_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    runs integer NOT NULL,
    PRIMARY KEY (repo, test_id)
);

CREATE TABLE test_results.quarantine_log (
    id bigserial PRIMARY KEY,
    repo text NOT NULL,
    test_id text NOT NULL,
    action text NOT NULL
        CHECK (action IN ('quarantined', 'released', 'expired', 'broken')),
    at timestamptz NOT NULL,
    detail jsonb NOT NULL
);
CREATE INDEX quarantine_log_test ON test_results.quarantine_log (repo, test_id, at);
CREATE INDEX quarantine_log_repo ON test_results.quarantine_log (repo, id);
CREATE INDEX quarantine_log_at ON test_results.quarantine_log (at);
"#;

/// What one pass changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub quarantined: u64,
    pub released: u64,
    pub expired: u64,
    /// Quarantined tests that became broken.
    pub broken: u64,
}

/// A log row as the classifier wrote it, with the test it names.
struct Logged {
    action: String,
    repo: String,
    at: DateTime<Utc>,
    detail: Value,
    test: Value,
}

/// Bring quarantine up to date as of `window.to`. Returns what changed and
/// the notifications to send for it.
pub(super) async fn update(
    transaction: &tokio_postgres::Transaction<'_>,
    window: &Window,
    rules: QuarantineRules,
) -> Result<(Changes, Vec<Notification>)> {
    let to = window.to_literal();
    let mut ids: Vec<i64> = Vec::new();
    let mut log = |rows: Vec<tokio_postgres::Row>| {
        ids.extend(rows.iter().map(|row| row.get::<_, i64>(0)));
        rows.len() as u64
    };

    // 1. Broken tests leave.
    let broken = log(transaction
        .query(
            &format!(
                "WITH gone AS ( \
                     DELETE FROM test_results.quarantine q USING test_results.tests t \
                     WHERE t.repo = q.repo AND t.test_id = q.test_id AND t.state = 'broken' \
                     RETURNING q.repo, q.test_id, q.entered_at, t.broken_since_sha) \
                 INSERT INTO test_results.quarantine_log (repo, test_id, action, at, detail) \
                 SELECT repo, test_id, 'broken', {to}, \
                        jsonb_build_object('entered_at', entered_at, \
                                           'broken_since_sha', broken_since_sha) \
                 FROM gone RETURNING id"
            ),
            &[],
        )
        .await?);

    // 2. Old entries expire.
    let expired = log(transaction
        .query(
            &format!(
                "WITH gone AS ( \
                     DELETE FROM test_results.quarantine WHERE expires_at <= {to} \
                     RETURNING repo, test_id, entered_at) \
                 INSERT INTO test_results.quarantine_log (repo, test_id, action, at, detail) \
                 SELECT repo, test_id, 'expired', {to}, \
                        jsonb_build_object('entered_at', entered_at) \
                 FROM gone RETURNING id"
            ),
            &[],
        )
        .await?);

    // 3. A run of passes since entering releases.
    let since_entry = window.finals(
        &format!(
            "tr.repo = q.repo AND tr.test_id = q.test_id \
             AND tr.received_on >= (q.entered_at AT TIME ZONE 'UTC')::date \
             AND r.received_at > q.entered_at AND r.received_at <= {to}"
        ),
        false,
    );
    let released = log(transaction
        .query(
            &format!(
                "WITH streaks AS ( \
                     SELECT q.repo, q.test_id, q.entered_at, \
                            (SELECT count(*) FROM ( \
                                 SELECT bool_and(NOT f.is_fail) OVER ( \
                                            ORDER BY f.received_at DESC, f.run_id DESC) AS clean \
                                 FROM ({since_entry}) f) s \
                             WHERE s.clean) AS passes \
                     FROM test_results.quarantine q), \
                 gone AS ( \
                     DELETE FROM test_results.quarantine q USING streaks s \
                     WHERE s.repo = q.repo AND s.test_id = q.test_id \
                       AND s.passes >= {passes} \
                     RETURNING q.repo, q.test_id, q.entered_at, s.passes) \
                 INSERT INTO test_results.quarantine_log (repo, test_id, action, at, detail) \
                 SELECT repo, test_id, 'released', {to}, \
                        jsonb_build_object('entered_at', entered_at, 'passes', passes) \
                 FROM gone RETURNING id",
                passes = rules.release_passes,
            ),
            &[],
        )
        .await?);

    // 4. Proven flakes enter.
    let mut quarantined = 0;
    if rules.runs > 0 {
        let from_day = (window.to - chrono::Duration::days(ENTRY_DAYS)).date_naive();
        quarantined = log(transaction
            .query(
                &format!(
                    "WITH left_at AS ( \
                         SELECT repo, test_id, max(at) AS at FROM test_results.quarantine_log \
                         WHERE action <> 'quarantined' GROUP BY repo, test_id), \
                     proven AS ( \
                         SELECT e.repo, e.test_id, count(DISTINCT e.run_id)::int AS runs, \
                                count(*)::int AS events \
                         FROM test_results.test_flake_events e \
                         LEFT JOIN left_at l ON l.repo = e.repo AND l.test_id = e.test_id \
                         WHERE e.kind = 'definite' AND e.observed_on >= '{from_day}'::date \
                           AND e.observed_at > {to} - interval '{ENTRY_DAYS} days' \
                           AND e.observed_at <= {to} \
                           AND (l.at IS NULL OR e.observed_at > l.at) \
                         GROUP BY e.repo, e.test_id \
                         HAVING count(DISTINCT e.run_id) >= {runs}), \
                     entered AS ( \
                         INSERT INTO test_results.quarantine \
                             (repo, test_id, entered_at, expires_at, runs) \
                         SELECT p.repo, p.test_id, {to}, {to} + interval '{days} days', p.runs \
                         FROM proven p \
                         JOIN test_results.tests t \
                              ON t.repo = p.repo AND t.test_id = p.test_id \
                             AND t.state <> 'broken' \
                         ON CONFLICT (repo, test_id) DO NOTHING \
                         RETURNING repo, test_id, runs, expires_at) \
                     INSERT INTO test_results.quarantine_log (repo, test_id, action, at, detail) \
                     SELECT n.repo, n.test_id, 'quarantined', {to}, \
                            jsonb_build_object('runs', n.runs, 'events', p.events, \
                                               'expires_at', n.expires_at) \
                     FROM entered n JOIN proven p USING (repo, test_id) RETURNING id",
                    runs = rules.runs,
                    days = rules.days,
                ),
                &[],
            )
            .await?);
    }

    let changes = Changes {
        quarantined,
        released,
        expired,
        broken,
    };
    let logged = transaction
        .query(
            "SELECT l.action, l.repo, l.at, l.detail::text AS detail, l.test_id, t.target, t.file, t.suite, t.name \
             FROM test_results.quarantine_log l \
             LEFT JOIN test_results.tests t ON t.repo = l.repo AND t.test_id = l.test_id \
             WHERE l.id = ANY($1) ORDER BY l.id",
            &[&ids],
        )
        .await?
        .iter()
        .map(|row| Logged {
            action: row.get("action"),
            repo: row.get("repo"),
            at: row.get("at"),
            detail: serde_json::from_str(row.get("detail")).unwrap_or(Value::Null),
            test: test_json(row),
        })
        .collect::<Vec<_>>();
    let notifications = logged
        .into_iter()
        .filter_map(|entry| {
            // A quarantined test turning broken is announced as test.broken,
            // with every other test that broke.
            let event = match entry.action.as_str() {
                "quarantined" => "test.quarantined",
                "released" => "test.released",
                "expired" => "test.expired",
                _ => return None,
            };
            Some(notification(
                event,
                &entry.repo,
                entry.at,
                entry.test,
                entry.detail,
            ))
        })
        .collect();
    Ok((changes, notifications))
}

/// The test fields of a row holding `test_id`, `target`, `file`, `suite`
/// and `name`.
pub(super) fn test_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "test_id": row.get::<_, String>("test_id"),
        "target": row.get::<_, Option<String>>("target"),
        "file": row.get::<_, Option<String>>("file"),
        "suite": row.get::<_, Option<String>>("suite"),
        "name": row.get::<_, Option<String>>("name"),
    })
}

/// A webhook body: the event, when it happened, the repository, the test
/// and what the event adds.
pub(super) fn notification(
    event: &'static str,
    repo: &str,
    at: DateTime<Utc>,
    test: Value,
    detail: Value,
) -> Notification {
    Notification {
        event,
        body: json!({
            "event": event,
            "occurred_at": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "repo": repo,
            "test": test,
            "detail": detail,
        }),
    }
}
