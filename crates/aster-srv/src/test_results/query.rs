//! The read API over test state: `GET /v1/test-results/tests`,
//! `.../tests/:test_id`, `.../runs/:run_id/failures`,
//! `.../failures?github_run=` (the failures of every Aster run in one GitHub
//! Actions run), `.../clusters` (the last week's failures grouped by failure
//! hash) and `.../quarantine` (the tests Aster keeps from failing builds).
//!
//! A CI caller reads only its own repository's data; a signed-in person
//! names any served repository with `repo` (see `http::read_identity`). Lists page with an opaque
//! keyset cursor (hex of the last row's sort key), so a page costs the same
//! however deep it is.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

use crate::auth::Identity;
use crate::http::Reply;
use crate::ui::data;
use crate::App;

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;
/// The most failures one run's report lists in a page.
const MAX_FAILURES: i64 = 1000;
/// The most tests the quarantine list returns; past it the list is cut
/// (and says so), which only lets more tests fail.
const MAX_QUARANTINED: i64 = 10_000;

/// A read route.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Tests,
    Test(String),
    RunFailures(String),
    GithubFailures,
    Quarantine,
    Clusters,
}

/// The route for a request path, with ids percent-decoded (a run id holds
/// `/`, so it arrives as `%2F`).
pub fn parse_route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/v1/test-results/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["tests"] => Some(Route::Tests),
        ["failures"] => Some(Route::GithubFailures),
        ["quarantine"] => Some(Route::Quarantine),
        ["clusters"] => Some(Route::Clusters),
        ["tests", id] if !id.is_empty() => Some(Route::Test(percent_decode(id)?)),
        ["runs", id, "failures"] if !id.is_empty() => Some(Route::RunFailures(percent_decode(id)?)),
        _ => None,
    }
}

pub(crate) fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = text.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn parse_query(query: Option<&str>) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((percent_decode(key)?, percent_decode(value)?));
    }
    Some(pairs)
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<String> {
    if text.len() & 1 == 1 || !text.is_ascii() {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

fn time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn opt_time(at: Option<DateTime<Utc>>) -> Value {
    at.map_or(Value::Null, |at| json!(time(at)))
}

/// The query parameters every route shares.
struct Params {
    repo: String,
    limit: i64,
    cursor: Option<String>,
    state: Option<String>,
    /// `github_run` and `attempt`, on the route that takes them.
    github_run: Option<String>,
    attempt: Option<i32>,
}

fn params(
    identity: &Identity,
    query: Option<&str>,
    max_limit: i64,
    default_limit: i64,
    github: bool,
) -> Result<Params, Reply> {
    let pairs = parse_query(query).ok_or_else(|| Reply::error(400, "the query is not valid"))?;
    let mut found = Params {
        repo: identity.repository.clone(),
        limit: default_limit,
        cursor: None,
        state: None,
        github_run: None,
        attempt: None,
    };
    for (key, value) in pairs {
        match key.as_str() {
            "repo" => {
                if !value.eq_ignore_ascii_case(&identity.repository) {
                    return Err(Reply::error(
                        403,
                        format!(
                            "the token is for {} but the request is for {value}",
                            identity.repository
                        ),
                    ));
                }
                // Data is stored under the spelling the token names.
            }
            "limit" => {
                found.limit = value
                    .parse()
                    .ok()
                    .filter(|limit| (1..=max_limit).contains(limit))
                    .ok_or_else(|| {
                        Reply::error(400, format!("limit must be from 1 to {max_limit}"))
                    })?;
            }
            "cursor" => {
                found.cursor = Some(
                    unhex(&value).ok_or_else(|| Reply::error(400, "the cursor is not valid"))?,
                )
            }
            "state" => {
                if !["flaky", "broken", "healthy", "quarantined"].contains(&value.as_str()) {
                    return Err(Reply::error(
                        400,
                        "state must be flaky, broken, healthy or quarantined",
                    ));
                }
                found.state = Some(value);
            }
            "github_run" if github => {
                if value.is_empty()
                    || value.len() > 20
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(Reply::error(400, "github_run must be a GitHub run id"));
                }
                found.github_run = Some(value);
            }
            "attempt" if github => {
                found.attempt = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|attempt| *attempt >= 1)
                        .ok_or_else(|| Reply::error(400, "attempt must be a positive number"))?,
                );
            }
            other => return Err(Reply::error(400, format!("unknown parameter {other}"))),
        }
    }
    Ok(found)
}

pub async fn handle(app: &App, identity: &Identity, route: Route, query: Option<&str>) -> Reply {
    let result = match route {
        Route::Tests => list_tests(app, identity, query).await,
        Route::Test(test_id) => test_detail(app, identity, &test_id, query).await,
        Route::RunFailures(run_id) => run_failures(app, identity, &run_id, query).await,
        Route::GithubFailures => github_failures(app, identity, query).await,
        Route::Quarantine => quarantine_list(app, identity, query).await,
        Route::Clusters => clusters(app, identity, query).await,
    };
    match result {
        Ok(reply) | Err(Failure::Reply(reply)) => reply,
        Err(Failure::Database(error)) => {
            crate::log::error(
                "cannot read test results",
                json!({"error": format!("{error:#}")}),
            );
            Reply::error(503, "the results could not be read; retry later")
        }
    }
}

enum Failure {
    Reply(Reply),
    Database(anyhow::Error),
}

impl From<Reply> for Failure {
    fn from(reply: Reply) -> Self {
        Failure::Reply(reply)
    }
}

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(error: E) -> Self {
        Failure::Database(error.into())
    }
}

type Handled = Result<Reply, Failure>;

const TEST_COLUMNS: &str =
    "test_id, target, file, suite, name, state, flake_score, last_flake_at, \
                            broken_since_sha, broken_failure_hash, first_seen, last_seen";

fn test_json(row: &Row) -> Value {
    json!({
        "test_id": row.get::<_, String>("test_id"),
        "target": row.get::<_, String>("target"),
        "file": row.get::<_, Option<String>>("file"),
        "suite": row.get::<_, Option<String>>("suite"),
        "name": row.get::<_, String>("name"),
        "state": row.get::<_, String>("state"),
        "flake_score": row.get::<_, i32>("flake_score"),
        "last_flake_at": opt_time(row.get("last_flake_at")),
        "broken_since_sha": row.get::<_, Option<String>>("broken_since_sha"),
        "broken_failure_hash": row.get::<_, Option<String>>("broken_failure_hash"),
        "first_seen": time(row.get("first_seen")),
        "last_seen": time(row.get("last_seen")),
    })
}

/// The list query: `repo` is `$1`; a state filter is `$2`; a cursor is the
/// next two parameters (score, test id); the limit is last. Sorted along
/// `tests_by_score` / `tests_by_state_score`. The state `quarantined` is
/// not a column but membership of the quarantine list, so its `$2` is
/// matched by an `EXISTS` on `quarantine` instead.
pub fn list_sql(with_state: bool, with_cursor: bool) -> String {
    let mut next = 2;
    let mut filters = String::new();
    if with_state {
        filters.push_str(&format!(
            " AND (CASE WHEN ${next} = 'quarantined' THEN {} ELSE state = ${next} END)",
            quarantine_filter()
        ));
        next += 1;
    }
    if with_cursor {
        filters.push_str(&format!(
            " AND flake_score <= ${next} AND (flake_score < ${next} OR test_id > ${})",
            next + 1
        ));
        next += 2;
    }
    format!(
        "SELECT {TEST_COLUMNS} FROM test_results.tests WHERE repo = $1{filters} \
         ORDER BY flake_score DESC, test_id LIMIT ${next}"
    )
}

/// Whether the `tests` row is on the quarantine list.
fn quarantine_filter() -> &'static str {
    "EXISTS (SELECT 1 FROM test_results.quarantine q \
             WHERE q.repo = tests.repo AND q.test_id = tests.test_id)"
}

/// `GET /v1/test-results/quarantine`: every quarantined test of the
/// caller's repository, and the list's version (its newest change).
async fn quarantine_list(app: &App, identity: &Identity, query: Option<&str>) -> Handled {
    let params = params(identity, query, 1, 1, false)?;
    if params.state.is_some() || params.cursor.is_some() || query.unwrap_or("").contains("limit") {
        return Err(Reply::error(400, "this route takes only repo").into());
    }
    let client = app.pool.get().await?;
    let version: i64 = client
        .query_one(
            "SELECT COALESCE(max(id), 0) FROM test_results.quarantine_log WHERE repo = $1",
            &[&params.repo],
        )
        .await?
        .get(0);
    let fetch = MAX_QUARANTINED + 1;
    let rows = client
        .query(
            "SELECT q.test_id, q.entered_at, q.expires_at, q.runs, \
                    t.target, t.file, t.suite, t.name \
             FROM test_results.quarantine q \
             LEFT JOIN test_results.tests t ON t.repo = q.repo AND t.test_id = q.test_id \
             WHERE q.repo = $1 ORDER BY q.test_id LIMIT $2",
            &[&params.repo, &fetch],
        )
        .await?;
    let truncated = rows.len() as i64 > MAX_QUARANTINED;
    let tests: Vec<Value> = rows
        .iter()
        .take(MAX_QUARANTINED as usize)
        .map(|row| {
            json!({
                "test_id": row.get::<_, String>("test_id"),
                "target": row.get::<_, Option<String>>("target"),
                "file": row.get::<_, Option<String>>("file"),
                "suite": row.get::<_, Option<String>>("suite"),
                "name": row.get::<_, Option<String>>("name"),
                "since": time(row.get("entered_at")),
                "expires_at": time(row.get("expires_at")),
                "runs": row.get::<_, i32>("runs"),
            })
        })
        .collect();
    Ok(Reply::json(
        200,
        json!({
            "repo": params.repo,
            "version": version,
            "generated_at": time(Utc::now()),
            "tests": tests,
            "truncated": truncated,
        }),
    ))
}

/// `GET /v1/test-results/clusters`: the failures of the last
/// [`data::CLUSTER_DAYS`] days grouped by failure hash, most frequent first;
/// what `/ui/clusters` shows, from the same query.
async fn clusters(app: &App, identity: &Identity, query: Option<&str>) -> Handled {
    let params = params(identity, query, 1, 1, false)?;
    if params.state.is_some() || params.cursor.is_some() || query.unwrap_or("").contains("limit") {
        return Err(Reply::error(400, "this route takes only repo").into());
    }
    let since = Utc::now().date_naive() - chrono::Days::new(data::CLUSTER_DAYS);
    let client = app.pool.get().await?;
    let clusters = data::clusters(&client, &params.repo, since).await?;
    let clusters: Vec<Value> = clusters
        .iter()
        .map(|cluster| {
            json!({
                "failure_hash": cluster.failure_hash,
                "failures": cluster.failures,
                "tests": cluster.tests,
                "first_seen": cluster.first_seen.to_string(),
                "last_seen": cluster.last_seen.to_string(),
                "example_test_id": cluster.example_test_id,
                "example_message": cluster.example_message,
            })
        })
        .collect();
    Ok(Reply::json(
        200,
        json!({
            "repo": params.repo,
            "since": since.to_string(),
            "days": data::CLUSTER_DAYS,
            "clusters": clusters,
        }),
    ))
}

async fn list_tests(app: &App, identity: &Identity, query: Option<&str>) -> Handled {
    let params = params(identity, query, MAX_LIMIT, DEFAULT_LIMIT, false)?;
    let cursor = match &params.cursor {
        Some(cursor) => {
            let (score, test_id) = cursor
                .split_once('\n')
                .and_then(|(score, id)| Some((score.parse::<i32>().ok()?, id.to_string())))
                .ok_or_else(|| Reply::error(400, "the cursor is not valid"))?;
            Some((score, test_id))
        }
        None => None,
    };
    let fetch = params.limit + 1;
    let mut args: Vec<&(dyn ToSql + Sync)> = vec![&params.repo];
    if let Some(state) = &params.state {
        args.push(state);
    }
    if let Some((score, test_id)) = &cursor {
        args.push(score);
        args.push(test_id);
    }
    args.push(&fetch);
    let sql = list_sql(params.state.is_some(), cursor.is_some());
    let client = app.pool.get().await?;
    let rows = client.query(&sql, &args).await?;
    let more = rows.len() as i64 > params.limit;
    let page = &rows[..rows.len().min(params.limit as usize)];
    let next = match page.last() {
        Some(last) if more => json!(hex(&format!(
            "{}\n{}",
            last.get::<_, i32>("flake_score"),
            last.get::<_, String>("test_id")
        ))),
        _ => Value::Null,
    };
    Ok(Reply::json(
        200,
        json!({
            "repo": params.repo,
            "tests": page.iter().map(test_json).collect::<Vec<_>>(),
            "next_cursor": next,
        }),
    ))
}

async fn test_detail(
    app: &App,
    identity: &Identity,
    test_id: &str,
    query: Option<&str>,
) -> Handled {
    let params = params(identity, query, MAX_LIMIT, DEFAULT_LIMIT, false)?;
    if params.state.is_some() {
        return Err(Reply::error(400, "state is not a parameter of this route").into());
    }
    let cursor = match &params.cursor {
        Some(cursor) => {
            let mut parts = cursor.splitn(3, '\n');
            let at = parts
                .next()
                .and_then(|micros| micros.parse::<i64>().ok())
                .and_then(DateTime::from_timestamp_micros);
            let run_id = parts.next().map(str::to_string);
            let attempt = parts.next().and_then(|a| a.parse::<i32>().ok());
            match (at, run_id, attempt) {
                (Some(at), Some(run_id), Some(attempt)) => Some((at, run_id, attempt)),
                _ => return Err(Reply::error(400, "the cursor is not valid").into()),
            }
        }
        None => None,
    };
    let client = app.pool.get().await?;
    let Some(test) = client
        .query_opt(
            &format!(
                "SELECT {TEST_COLUMNS} FROM test_results.tests WHERE repo = $1 AND test_id = $2"
            ),
            &[&params.repo, &test_id],
        )
        .await?
    else {
        return Err(Reply::error(404, "no such test").into());
    };

    let since = Utc::now().date_naive() - chrono::Days::new(u64::from(app.config.retention_days));
    let fetch = params.limit + 1;
    let mut args: Vec<&(dyn ToSql + Sync)> = vec![&params.repo, &test_id, &since];
    if let Some((at, run_id, attempt)) = &cursor {
        args.push(at);
        args.push(run_id);
        args.push(attempt);
    }
    args.push(&fetch);
    let rows = client.query(&history_sql(cursor.is_some()), &args).await?;
    let more = rows.len() as i64 > params.limit;
    let page = &rows[..rows.len().min(params.limit as usize)];
    let next = match page.last() {
        Some(last) if more => {
            let at: DateTime<Utc> = last.get("observed_at");
            json!(hex(&format!(
                "{}\n{}\n{}",
                at.timestamp_micros(),
                last.get::<_, String>("run_id"),
                last.get::<_, i32>("attempt")
            )))
        }
        _ => Value::Null,
    };
    let history: Vec<Value> = page
        .iter()
        .map(|row| {
            json!({
                "run_id": row.get::<_, String>("run_id"),
                "sha": row.get::<_, String>("sha"),
                "branch": row.get::<_, Option<String>>("branch"),
                "attempt": row.get::<_, i32>("attempt"),
                "outcome": row.get::<_, String>("outcome"),
                "duration_ms": row.get::<_, Option<i64>>("duration_ms"),
                "failure_hash": row.get::<_, Option<String>>("failure_hash"),
                "observed_at": time(row.get("observed_at")),
            })
        })
        .collect();
    Ok(Reply::json(
        200,
        json!({
            "repo": params.repo,
            "test": test_json(&test),
            "history": history,
            "next_cursor": next,
        }),
    ))
}

/// A test's results, newest first: `repo`, `test_id` and the oldest day
/// kept are `$1`-`$3`; a cursor is the next three; the limit is last.
pub fn history_sql(with_cursor: bool) -> String {
    let (filter, limit) = if with_cursor {
        (
            " AND (r.received_at, tr.run_id, tr.attempt) < ($4, $5, $6)",
            "$7",
        )
    } else {
        ("", "$4")
    };
    format!(
        "SELECT tr.run_id, r.sha, r.branch, tr.attempt, tr.outcome, tr.duration_ms, \
                tr.failure_hash, r.received_at AS observed_at \
         FROM test_results.test_results tr \
         JOIN test_results.runs r ON r.run_id = tr.run_id \
         WHERE tr.repo = $1 AND tr.test_id = $2 AND tr.received_on >= $3{filter} \
         ORDER BY r.received_at DESC, tr.run_id DESC, tr.attempt DESC LIMIT {limit}"
    )
}

async fn run_failures(
    app: &App,
    identity: &Identity,
    run_id: &str,
    query: Option<&str>,
) -> Handled {
    let params = params(identity, query, MAX_FAILURES, MAX_FAILURES, false)?;
    if params.state.is_some() {
        return Err(Reply::error(400, "state is not a parameter of this route").into());
    }
    let client = app.pool.get().await?;
    let Some(run) = client
        .query_opt(
            "SELECT repo, sha, branch, pr, received_at, \
                    (received_at AT TIME ZONE 'UTC')::date AS received_on \
             FROM test_results.runs WHERE run_id = $1",
            &[&run_id],
        )
        .await?
    else {
        return Err(Reply::error(404, "no such run").into());
    };
    let repo: String = run.get("repo");
    if !repo.eq_ignore_ascii_case(&identity.repository) {
        return Err(Reply::error(
            403,
            format!("the run belongs to {repo}, not {}", identity.repository),
        )
        .into());
    }
    let received_on: chrono::NaiveDate = run.get("received_on");
    let after = params.cursor.clone().unwrap_or_default();
    let fetch = params.limit + 1;
    let rows = client
        .query(
            &failures_sql(),
            &[
                &vec![run_id.to_string()],
                &vec![received_on],
                &run_id,
                &after,
                &fetch,
                &repo,
            ],
        )
        .await?;
    let more = rows.len() as i64 > params.limit;
    let page = &rows[..rows.len().min(params.limit as usize)];
    let failures: Vec<Value> = page.iter().map(failure_json).collect();
    let next = match page.last() {
        Some(last) if more => json!(hex(&last.get::<_, String>("test_id"))),
        _ => Value::Null,
    };
    Ok(Reply::json(
        200,
        json!({
            "run": {
                "run_id": run_id,
                "repo": repo,
                "sha": run.get::<_, String>("sha"),
                "branch": run.get::<_, Option<String>>("branch"),
                "pr": run.get::<_, Option<i64>>("pr"),
                "received_at": time(run.get("received_at")),
            },
            "failures": failures,
            "next_cursor": next,
        }),
    ))
}

/// The failing tests of some runs, ordered by run then test. Parameters:
/// `$1` run ids and `$2` their `received_on` days (parallel arrays), `$3`/`$4`
/// the run id and test id to continue after, `$5` the row limit and `$6` the
/// repository. A run has one `failing` row per test, whatever its attempts.
fn failures_sql() -> String {
    "WITH wanted AS (SELECT * FROM unnest($1::text[], $2::date[]) AS w(run_id, received_on)), \
          failing AS MATERIALIZED ( \
             SELECT DISTINCT tr.run_id, tr.received_on, tr.test_id \
             FROM wanted w \
             JOIN test_results.test_results tr \
                  ON tr.run_id = w.run_id AND tr.received_on = w.received_on \
             WHERE tr.repo = $6 AND tr.outcome IN ('fail', 'error') \
               AND (tr.run_id, tr.test_id) > ($3, $4) \
             ORDER BY tr.run_id, tr.test_id LIMIT $5) \
     SELECT tr.run_id, tr.test_id, max(tr.target) AS target, \
            array_agg(tr.attempt ORDER BY tr.attempt) AS attempts, \
            array_agg(tr.outcome ORDER BY tr.attempt) AS outcomes, \
            array_agg(tr.duration_ms ORDER BY tr.attempt) AS durations, \
            (array_agg(tr.failure_hash ORDER BY tr.attempt DESC) \
                 FILTER (WHERE tr.outcome IN ('fail', 'error')))[1] AS failure_hash, \
            (array_agg(tr.message ORDER BY tr.attempt DESC) \
                 FILTER (WHERE tr.outcome IN ('fail', 'error')))[1] AS message, \
            t.name, t.file, t.suite, t.state, t.flake_score, \
            t.broken_since_sha, t.last_flake_at \
     FROM failing f \
     JOIN test_results.test_results tr \
          ON tr.run_id = f.run_id AND tr.received_on = f.received_on AND tr.repo = $6 \
         AND tr.test_id = f.test_id \
     LEFT JOIN test_results.tests t ON t.repo = $6 AND t.test_id = f.test_id \
     GROUP BY tr.run_id, tr.test_id, t.name, t.file, t.suite, t.state, t.flake_score, \
              t.broken_since_sha, t.last_flake_at \
     ORDER BY tr.run_id, tr.test_id"
        .to_string()
}

/// One failing test as the failure reports show it.
fn failure_json(row: &Row) -> Value {
    let attempts: Vec<i32> = row.get("attempts");
    let outcomes: Vec<String> = row.get("outcomes");
    let durations: Vec<Option<i64>> = row.get("durations");
    // A test the classifier has not seen yet is as new as one it judged
    // healthy.
    let state = match row.get::<_, Option<String>>("state").as_deref() {
        Some("flaky") => "flaky",
        Some("broken") => "broken",
        _ => "new",
    };
    json!({
        "test_id": row.get::<_, String>("test_id"),
        "name": row.get::<_, Option<String>>("name"),
        "file": row.get::<_, Option<String>>("file"),
        "suite": row.get::<_, Option<String>>("suite"),
        "target": row.get::<_, String>("target"),
        "state": state,
        "flake_score": row.get::<_, Option<i32>>("flake_score").unwrap_or(0),
        "last_flake_at": opt_time(row.get("last_flake_at")),
        "broken_since_sha": row.get::<_, Option<String>>("broken_since_sha"),
        "failure_hash": row.get::<_, Option<String>>("failure_hash"),
        "message": row.get::<_, Option<String>>("message"),
        "final_outcome": outcomes.last(),
        "passed_on_retry": outcomes.last().is_some_and(|o| o == "pass"),
        "attempts": attempts
            .iter()
            .zip(&outcomes)
            .zip(&durations)
            .map(|((attempt, outcome), duration)| {
                json!({"attempt": attempt, "outcome": outcome, "duration_ms": duration})
            })
            .collect::<Vec<_>>(),
    })
}

/// The `LIKE` pattern for the `run_id`s of one GitHub Actions run (and
/// attempt), as `detect_run` builds them: `gh:<repo>:<run>:<attempt>:...`.
/// The fixed part is escaped, then `%` matches the rest.
pub fn github_run_pattern(repo: &str, github_run: &str, attempt: Option<i32>) -> String {
    let mut prefix = format!("gh:{repo}:{github_run}:");
    if let Some(attempt) = attempt {
        prefix.push_str(&format!("{attempt}:"));
    }
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// `GET /v1/test-results/failures?github_run=`: the runs Aster recorded for
/// one GitHub Actions run, and their failing tests, merged.
async fn github_failures(app: &App, identity: &Identity, query: Option<&str>) -> Handled {
    let params = params(identity, query, MAX_FAILURES, MAX_FAILURES, true)?;
    if params.state.is_some() {
        return Err(Reply::error(400, "state is not a parameter of this route").into());
    }
    let Some(github_run) = &params.github_run else {
        return Err(Reply::error(400, "github_run is required").into());
    };
    let (after_run, after_test) = match &params.cursor {
        Some(cursor) => cursor
            .split_once('\n')
            .map(|(run, test)| (run.to_string(), test.to_string()))
            .ok_or_else(|| Reply::error(400, "the cursor is not valid"))?,
        None => (String::new(), String::new()),
    };
    let pattern = github_run_pattern(&params.repo, github_run, params.attempt);
    let since = Utc::now() - chrono::Days::new(u64::from(app.config.retention_days));
    let client = app.pool.get().await?;
    let runs = client
        .query(
            "SELECT run_id, job, lane, runner, sha, branch, pr, attempt, received_at, \
                    (received_at AT TIME ZONE 'UTC')::date AS received_on \
             FROM test_results.runs \
             WHERE repo = $1 AND received_at >= $2 AND run_id ILIKE $3 ESCAPE '\\' \
             ORDER BY run_id LIMIT 500",
            &[&params.repo, &since, &pattern],
        )
        .await?;
    if runs.is_empty() {
        return Err(Reply::error(404, "no runs recorded for that GitHub run").into());
    }
    let run_ids: Vec<String> = runs.iter().map(|run| run.get("run_id")).collect();
    let days: Vec<chrono::NaiveDate> = runs.iter().map(|run| run.get("received_on")).collect();
    let fetch = params.limit + 1;
    let rows = client
        .query(
            &failures_sql(),
            &[
                &run_ids,
                &days,
                &after_run,
                &after_test,
                &fetch,
                &params.repo,
            ],
        )
        .await?;
    let more = rows.len() as i64 > params.limit;
    let page = &rows[..rows.len().min(params.limit as usize)];
    let failures: Vec<Value> = page
        .iter()
        .map(|row| {
            let mut failure = failure_json(row);
            let run_id: String = row.get("run_id");
            let run = runs
                .iter()
                .find(|run| run.get::<_, String>("run_id") == run_id);
            failure["run_id"] = json!(run_id);
            failure["job"] = json!(run.and_then(|run| run.get::<_, Option<String>>("job")));
            failure["lane"] = json!(run.and_then(|run| run.get::<_, Option<String>>("lane")));
            failure["runner"] = json!(run.and_then(|run| run.get::<_, Option<String>>("runner")));
            failure
        })
        .collect();
    let next = match page.last() {
        Some(last) if more => json!(hex(&format!(
            "{}\n{}",
            last.get::<_, String>("run_id"),
            last.get::<_, String>("test_id")
        ))),
        _ => Value::Null,
    };
    let runs: Vec<Value> = runs
        .iter()
        .map(|run| {
            json!({
                "run_id": run.get::<_, String>("run_id"),
                "job": run.get::<_, Option<String>>("job"),
                "lane": run.get::<_, Option<String>>("lane"),
                "runner": run.get::<_, Option<String>>("runner"),
                "sha": run.get::<_, String>("sha"),
                "branch": run.get::<_, Option<String>>("branch"),
                "pr": run.get::<_, Option<i64>>("pr"),
                "attempt": run.get::<_, Option<i32>>("attempt"),
                "received_at": time(run.get("received_at")),
            })
        })
        .collect();
    Ok(Reply::json(
        200,
        json!({
            "github_run": github_run,
            "runs": runs,
            "failures": failures,
            "next_cursor": next,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_decode_ids() {
        assert_eq!(parse_route("/v1/test-results/tests"), Some(Route::Tests));
        assert_eq!(
            parse_route("/v1/test-results/tests/ab%2Fc"),
            Some(Route::Test("ab/c".into()))
        );
        assert_eq!(
            parse_route("/v1/test-results/runs/gh:A%2Fb:1:1/failures"),
            Some(Route::RunFailures("gh:A/b:1:1".into()))
        );
        assert_eq!(
            parse_route("/v1/test-results/failures"),
            Some(Route::GithubFailures)
        );
        for path in [
            "/v1/test-results/failures/x",
            "/v1/test-results/runs",
            "/v1/test-results/tests/",
            "/v1/test-results/runs/x",
            "/v1/test-results/tests/a/b",
            "/v1/test-results/tests/%zz",
        ] {
            assert_eq!(parse_route(path), None, "{path}");
        }
    }

    #[test]
    fn cursors_round_trip() {
        assert_eq!(unhex(&hex("3\nabc")).as_deref(), Some("3\nabc"));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn github_run_patterns_escape_like_metacharacters() {
        assert_eq!(github_run_pattern("a/b", "123", None), "gh:a/b:123:%");
        assert_eq!(
            github_run_pattern("Arch_Astro/we%ird", "9", Some(2)),
            r"gh:Arch\_Astro/we\%ird:9:2:%"
        );
    }

    #[test]
    fn list_sql_numbers_its_parameters() {
        assert!(list_sql(false, false).ends_with("LIMIT $2"));
        assert!(list_sql(true, true).contains("state = $2"));
        assert!(list_sql(true, true).ends_with("LIMIT $5"));
    }
}
