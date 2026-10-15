//! What the dashboard reads. Every query is bounded by repository, by the
//! indexes on `tests` and `test_results`, and by a LIMIT, so a page costs
//! about the same however much history there is.

use chrono::{DateTime, NaiveDate, Utc};
use deadpool_postgres::Client;
use tokio_postgres::Row;

/// The most rows a leaderboard section lists.
pub const FLAKY_LIMIT: i64 = 200;
pub const SCORED_LIMIT: i64 = 100;
pub const BROKEN_LIMIT: i64 = 100;
pub const CLUSTER_LIMIT: i64 = 50;
/// The most results one test's page reads: the strip shows all of them, the
/// table the newest [`TABLE_ROWS`].
pub const HISTORY_LIMIT: i64 = 300;
pub const TABLE_ROWS: usize = 50;
/// Failure messages are cut here in SQL, before they leave the database.
const MESSAGE_CHARS: i32 = 2000;
/// How many days of failures the clusters page groups.
pub const CLUSTER_DAYS: u64 = 7;

pub type Result<T> = std::result::Result<T, tokio_postgres::Error>;

pub struct TestRow {
    pub test_id: String,
    pub target: String,
    pub file: Option<String>,
    pub name: String,
    pub state: String,
    pub flake_score: i32,
    pub last_flake_at: Option<DateTime<Utc>>,
    pub broken_since_sha: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl TestRow {
    fn from_row(row: &Row) -> Self {
        Self {
            test_id: row.get("test_id"),
            target: row.get("target"),
            file: row.get("file"),
            name: row.get("name"),
            state: row.get("state"),
            flake_score: row.get("flake_score"),
            last_flake_at: row.get("last_flake_at"),
            broken_since_sha: row.get("broken_since_sha"),
            first_seen: row.get("first_seen"),
            last_seen: row.get("last_seen"),
        }
    }
}

const TEST_COLUMNS: &str = "test_id, target, file, name, state, flake_score, last_flake_at, \
                            broken_since_sha, first_seen, last_seen";

/// Repositories that have runs, the one with the newest run first.
///
/// A loose index scan over `runs_repo_received`: one probe per repository
/// rather than a pass over every run.
pub async fn repos(client: &Client) -> Result<Vec<String>> {
    let rows = client
        .query(
            "WITH RECURSIVE r AS ( \
                 (SELECT repo, received_at FROM test_results.runs \
                  ORDER BY repo DESC, received_at DESC LIMIT 1) \
                 UNION ALL \
                 SELECT n.repo, n.received_at FROM r, LATERAL ( \
                     SELECT repo, received_at FROM test_results.runs \
                     WHERE repo < r.repo ORDER BY repo DESC, received_at DESC LIMIT 1) n) \
             SELECT repo FROM r ORDER BY received_at DESC, repo",
            &[],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

/// What the flaky leaderboard lists.
pub struct Leaderboard {
    /// Tests in the flaky state, highest score first.
    pub flaky: Vec<TestRow>,
    /// Healthy tests that still carry a flake score.
    pub scored: Vec<TestRow>,
    pub broken: Vec<TestRow>,
}

pub async fn leaderboard(client: &Client, repo: &str) -> Result<Leaderboard> {
    let by_state = |state: &'static str, extra: &'static str, limit: i64| {
        let sql = format!(
            "SELECT {TEST_COLUMNS} FROM test_results.tests \
             WHERE repo = $1 AND state = '{state}'{extra} \
             ORDER BY flake_score DESC, test_id LIMIT {limit}"
        );
        async move {
            client
                .query(&sql, &[&repo])
                .await
                .map(|rows| rows.iter().map(TestRow::from_row).collect::<Vec<_>>())
        }
    };
    let (flaky, scored, broken) = tokio::try_join!(
        by_state("flaky", "", FLAKY_LIMIT),
        by_state("healthy", " AND flake_score > 0", SCORED_LIMIT),
        by_state("broken", "", BROKEN_LIMIT),
    )?;
    Ok(Leaderboard {
        flaky,
        scored,
        broken,
    })
}

pub async fn test(client: &Client, repo: &str, test_id: &str) -> Result<Option<TestRow>> {
    Ok(client
        .query_opt(
            &format!(
                "SELECT {TEST_COLUMNS} FROM test_results.tests WHERE repo = $1 AND test_id = $2"
            ),
            &[&repo, &test_id],
        )
        .await?
        .map(|row| TestRow::from_row(&row)))
}

/// One attempt of a test in one run.
pub struct Attempt {
    pub sha: String,
    pub branch: Option<String>,
    pub attempt: i32,
    pub outcome: String,
    pub duration_ms: Option<i64>,
    pub message: Option<String>,
    pub at: DateTime<Utc>,
}

/// A test's newest results within the retention window, newest first.
pub async fn history(
    client: &Client,
    repo: &str,
    test_id: &str,
    since: NaiveDate,
) -> Result<Vec<Attempt>> {
    let rows = client
        .query(
            &format!(
                "SELECT r.sha, r.branch, tr.attempt, tr.outcome, tr.duration_ms, \
                        left(tr.message, {MESSAGE_CHARS}) AS message, r.received_at AS at \
                 FROM test_results.test_results tr \
                 JOIN test_results.runs r ON r.run_id = tr.run_id \
                 WHERE tr.repo = $1 AND tr.test_id = $2 AND tr.received_on >= $3 \
                 ORDER BY r.received_at DESC, tr.run_id DESC, tr.attempt DESC LIMIT {HISTORY_LIMIT}"
            ),
            &[&repo, &test_id, &since],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| Attempt {
            sha: row.get("sha"),
            branch: row.get("branch"),
            attempt: row.get("attempt"),
            outcome: row.get("outcome"),
            duration_ms: row.get("duration_ms"),
            message: row.get("message"),
            at: row.get("at"),
        })
        .collect())
}

/// Failures sharing a failure hash.
pub struct Cluster {
    pub failure_hash: String,
    pub failures: i64,
    pub tests: i64,
    pub first_seen: NaiveDate,
    pub last_seen: NaiveDate,
    pub example_test_id: String,
    pub example_message: Option<String>,
}

/// SQL for [`clusters`]: `repo` is `$1`, the oldest day counted `$2`.
///
/// `received_on` bounds it to a few partitions, and the partial index
/// `test_results_failures` (repo, received_on) finds the failing rows in
/// each without reading the passing ones. The example's message is fetched
/// for only the clusters listed, through `test_results_test`.
pub const CLUSTERS_SQL: &str = "\
SELECT c.failure_hash, c.failures, c.tests, c.first_seen, c.last_seen, c.example_test_id, \
       ex.message \
FROM ( \
    SELECT failure_hash, count(*) AS failures, count(DISTINCT test_id) AS tests, \
           min(received_on) AS first_seen, max(received_on) AS last_seen, \
           (array_agg(test_id ORDER BY received_on DESC))[1] AS example_test_id \
    FROM test_results.test_results \
    WHERE repo = $1 AND received_on >= $2 \
      AND outcome IN ('fail', 'error') AND failure_hash IS NOT NULL \
    GROUP BY failure_hash \
    ORDER BY count(*) DESC, failure_hash \
    LIMIT $3) c \
LEFT JOIN LATERAL ( \
    SELECT left(message, 2000) AS message FROM test_results.test_results \
    WHERE repo = $1 AND test_id = c.example_test_id AND received_on >= $2 \
      AND failure_hash = c.failure_hash AND outcome IN ('fail', 'error') \
    ORDER BY received_on DESC LIMIT 1) ex ON true \
ORDER BY c.failures DESC, c.failure_hash";

pub async fn clusters(client: &Client, repo: &str, since: NaiveDate) -> Result<Vec<Cluster>> {
    let rows = client
        .query(CLUSTERS_SQL, &[&repo, &since, &CLUSTER_LIMIT])
        .await?;
    Ok(rows
        .iter()
        .map(|row| Cluster {
            failure_hash: row.get("failure_hash"),
            failures: row.get("failures"),
            tests: row.get("tests"),
            first_seen: row.get("first_seen"),
            last_seen: row.get("last_seen"),
            example_test_id: row.get("example_test_id"),
            example_message: row.get("message"),
        })
        .collect())
}
