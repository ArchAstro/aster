//! `GET /v1/test-results/durations?target=&target=&day=`: how long each
//! test file takes, so Aster can deal a partitioned suite out by time
//! instead of by count.
//!
//! Every shard of one commit asks the same question and must get the same
//! answer, or a file runs on no shard or on two. So the answer depends only
//! on the request: `day` (the UTC day of the commit, the same on every
//! shard) fixes a window of runs received in the 7 days before that day
//! began, less a 15-minute margin for uploads still committing when it
//! began. A window that may still be filling (`day` is in the future) is
//! refused with 409, never answered from partial data.
//!
//! Per file: the sum of its tests' first-attempt durations in a run, then
//! the median of that over the most recent [`RUNS_PER_TARGET`] uncached,
//! completely captured runs of each target in the window. Files are named
//! as the runner reported them, relative to the directory the tests ran in.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};

use super::query::parse_query;
use crate::auth::Identity;
use crate::http::Reply;
use crate::App;

/// The most targets one request names: the shards of one suite.
pub const MAX_TARGETS: usize = 32;
/// Runs of each target the median is taken over.
pub const RUNS_PER_TARGET: i64 = 20;
/// Days of runs before `day` that count.
pub const WINDOW_DAYS: i64 = 7;
/// Answers a replica keeps; past this it forgets them all.
const CACHED_ANSWERS: usize = 256;

/// Answers already given, by repository, targets and day.
pub type Answers = Mutex<HashMap<(String, Vec<String>, NaiveDate), Value>>;

/// Runs received this close before `day` began are left out: their upload
/// may still have been committing.
const SETTLE: Duration = Duration::minutes(15);

/// The window for `day`: runs with `received_at` in `[from, to)`.
pub fn window(day: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = day.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
    let to = start - SETTLE;
    (to - Duration::days(WINDOW_DAYS), to)
}

/// The query: repo `$1`, targets `$2`, window `$3`..`$4`, days `$5`..`$6`
/// (the partitions that can hold the window), runs per target `$7`.
///
/// The picked runs (at most [`MAX_TARGETS`] × [`RUNS_PER_TARGET`]) drive
/// the read of their results through `test_results_run`, one partition
/// each; `OFFSET 0` keeps the planner from turning that into a scan of
/// every partition. On a seeded 14M-row copy, 4 targets of 3,000 tests
/// take 1 to 2.5 s (the scan took over 10 minutes), so an answer is kept
/// once computed: it cannot change.
pub const SQL: &str = "\
WITH recent AS ( \
    SELECT t.run_id, t.address, t.received_on, \
           row_number() OVER (PARTITION BY t.address \
                              ORDER BY r.received_at DESC, t.run_id DESC) AS n \
    FROM test_results.target_runs t \
    JOIN test_results.runs r ON r.run_id = t.run_id \
    WHERE t.repo = $1 AND t.address = ANY($2) AND NOT t.cached \
      AND t.confidence = 'complete' \
      AND t.received_on BETWEEN $5 AND $6 \
      AND r.received_at >= $3 AND r.received_at < $4), \
picked AS MATERIALIZED (SELECT run_id, address, received_on FROM recent WHERE n <= $7), \
per_test AS MATERIALIZED ( \
    SELECT p.run_id, tr.repo, tr.test_id, tr.duration_ms \
    FROM picked p \
    CROSS JOIN LATERAL ( \
        SELECT tr.repo, tr.test_id, tr.duration_ms FROM test_results.test_results tr \
        WHERE tr.run_id = p.run_id AND tr.received_on = p.received_on \
          AND tr.target = p.address AND tr.attempt = 1 AND tr.outcome <> 'skip' \
          AND tr.duration_ms IS NOT NULL \
        OFFSET 0) tr), \
per_run AS ( \
    SELECT t.run_id, s.file, sum(t.duration_ms) AS ms \
    FROM per_test t \
    JOIN test_results.tests s ON s.repo = t.repo AND s.test_id = t.test_id \
    WHERE s.file IS NOT NULL AND s.file <> '' \
    GROUP BY t.run_id, s.file) \
SELECT file, \
       round(percentile_cont(0.5) WITHIN GROUP (ORDER BY ms))::bigint AS p50_ms, \
       count(*) AS runs \
FROM per_run GROUP BY file ORDER BY file";

pub async fn handle(app: &App, identity: &Identity, query: Option<&str>) -> Reply {
    let (targets, day) = match parse(identity, query) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    if day > Utc::now().date_naive() {
        return Reply::error(
            409,
            format!("{day} has not begun, so its window may still be filling"),
        );
    }
    let key = (identity.repository.clone(), targets, day);
    if let Some(body) = app.durations.lock().expect("answers").get(&key) {
        return Reply::json(200, body.clone());
    }
    match read(app, &key.0, &key.1, day).await {
        Ok(body) => {
            let mut kept = app.durations.lock().expect("answers");
            if kept.len() >= CACHED_ANSWERS {
                kept.clear();
            }
            kept.insert(key, body.clone());
            Reply::json(200, body)
        }
        Err(error) => {
            crate::log::error(
                "cannot read test durations",
                json!({"error": format!("{error:#}")}),
            );
            Reply::error(503, "the durations could not be read; retry later")
        }
    }
}

fn parse(identity: &Identity, query: Option<&str>) -> Result<(Vec<String>, NaiveDate), Reply> {
    let pairs = parse_query(query).ok_or_else(|| Reply::error(400, "the query is not valid"))?;
    let mut targets = Vec::new();
    let mut day = None;
    for (key, value) in pairs {
        match key.as_str() {
            "repo" if !value.eq_ignore_ascii_case(&identity.repository) => {
                return Err(Reply::error(
                    403,
                    format!(
                        "the token is for {} but the request is for {value}",
                        identity.repository
                    ),
                ));
            }
            "repo" => {}
            "target" if value.starts_with("//") && value.contains(':') => {
                if !targets.contains(&value) {
                    targets.push(value);
                }
            }
            "target" => return Err(Reply::error(400, "target must be a target address")),
            "day" => {
                day = Some(
                    NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                        .map_err(|_| Reply::error(400, "day must be a date (YYYY-MM-DD)"))?,
                );
            }
            other => return Err(Reply::error(400, format!("unknown parameter {other}"))),
        }
    }
    if targets.is_empty() || targets.len() > MAX_TARGETS {
        return Err(Reply::error(
            400,
            format!("name from 1 to {MAX_TARGETS} targets"),
        ));
    }
    let day = day.ok_or_else(|| Reply::error(400, "day is required"))?;
    targets.sort();
    Ok((targets, day))
}

async fn read(app: &App, repo: &str, targets: &[String], day: NaiveDate) -> anyhow::Result<Value> {
    let (from, to) = window(day);
    let (first_day, last_day) = (from.date_naive(), to.date_naive());
    let client = app.pool.get().await?;
    let rows = client
        .query(
            SQL,
            &[
                &repo,
                &targets,
                &from,
                &to,
                &first_day,
                &last_day,
                &RUNS_PER_TARGET,
            ],
        )
        .await?;
    let files: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "file": row.get::<_, String>("file"),
                "p50_ms": row.get::<_, i64>("p50_ms"),
                "runs": row.get::<_, i64>("runs"),
            })
        })
        .collect();
    Ok(json!({
        "repo": repo,
        "targets": targets,
        "day": day.to_string(),
        "files": files,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_ends_before_the_day_begins() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let (from, to) = window(day);
        assert_eq!(to.to_rfc3339(), "2026-10-14T23:45:00+00:00");
        assert_eq!(from.to_rfc3339(), "2026-10-07T23:45:00+00:00");
    }

    #[test]
    fn parses_targets_and_day() {
        let identity = Identity {
            repository: "ArchAstro/firstlanding".into(),
            workflow_ref: None,
            static_token: false,
        };
        let (targets, day) = parse(
            &identity,
            Some("target=%2F%2Fb%3Atest&target=%2F%2Fa%3Atest&day=2026-10-15&repo=archastro%2Ffirstlanding"),
        )
        .unwrap();
        assert_eq!(targets, ["//a:test", "//b:test"]);
        assert_eq!(day.to_string(), "2026-10-15");
        for bad in [
            "day=2026-10-15",
            "target=%2F%2Fa%3Atest",
            "target=a&day=2026-10-15",
            "target=%2F%2Fa%3Atest&day=yesterday",
            "target=%2F%2Fa%3Atest&day=2026-10-15&limit=3",
        ] {
            assert_eq!(
                parse(&identity, Some(bad)).unwrap_err().status,
                400,
                "{bad}"
            );
        }
        let other = "target=%2F%2Fa%3Atest&day=2026-10-15&repo=ArchAstro%2Fother";
        assert_eq!(parse(&identity, Some(other)).unwrap_err().status, 403);
    }
}
