//! Read-only browsing of the cache: `GET /v1/cache/summary` and
//! `GET /v1/cache/entries`.
//!
//! Callers are an IAP-verified viewer (the `ASTER_SRV_UI_EMAIL_DOMAINS` rule)
//! or a CI token for its own repository. The repository must pass the
//! server's allowlist. The dashboard's `/ui/cache` page reads through
//! [`read_summary`] and [`read_entries`].

use base64::Engine;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::policy;
use super::routes::{db_error, Headers};
use crate::auth::AuthError;
use crate::iap::ViewerError;
use crate::test_results::query::parse_query;
use crate::App;
use crate::Reply;

/// Entries in a page unless `limit` says otherwise.
pub const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
/// Days of lookup counts in a summary.
pub const LOOKUP_DAYS: i64 = 14;
/// With a `target` filter the substring is matched here, not in SQL (a
/// stored manifest need not be valid JSON), so a page reads rows in batches
/// and stops after this many. `next` then resumes from the last row read.
const SCAN_BATCH: i64 = 200;
const SCAN_CAP: usize = 2000;

/// Who wrote a manifest. Each field is null for a static token.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WriterOut {
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub run_id: Option<String>,
    pub sha: Option<String>,
    pub workflow: Option<String>,
}

/// One day of lookup counts in a summary.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LookupDay {
    pub day: String,
    pub hits: i64,
    pub misses: i64,
}

/// What `summary` reports; see [`summary`] for the meaning of each field.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Summary {
    pub repo: String,
    pub manifests: i64,
    pub manifest_bytes: i64,
    pub blobs: i64,
    pub blob_bytes: i64,
    pub last_write_at: Option<String>,
    pub last_writer: Option<WriterOut>,
    pub lookups: Vec<LookupDay>,
}

/// One manifest in a page of `entries`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub target: String,
    pub size_bytes: i64,
    pub output_files: u64,
    pub exit_code: i64,
    pub duration_ms: u64,
    pub aster_version: String,
    pub created_at: String,
    pub last_used_at: String,
    pub hits: i64,
    pub last_hit_at: Option<String>,
    pub writer: WriterOut,
}

/// Where the next page starts: after this `(created_at, key)` in the order
/// `created_at DESC, key ASC`. Sent as unpadded URL-safe base64 of JSON, and
/// meant to be opaque to clients.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// `created_at` as microseconds since the Unix epoch, the precision
    /// Postgres stores, so the comparison is exact.
    t: i64,
    k: String,
}

impl Cursor {
    fn new(created_at: DateTime<Utc>, key: &str) -> Self {
        Self {
            t: created_at.timestamp_micros(),
            k: key.to_string(),
        }
    }

    pub fn encode(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).expect("a cursor is JSON"))
    }

    pub fn decode(text: &str) -> Option<Self> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The cursor `text` encodes, if this server could have issued it.
    pub fn parse(text: &str) -> Option<Self> {
        Self::decode(text).filter(|cursor| cursor.created_at().is_some())
    }

    fn created_at(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp_micros(self.t)
    }
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Check the caller may browse `repo`'s cache.
async fn authorize(app: &App, headers: &Headers<'_>, repo: &str) -> Result<(), Reply> {
    if let Some(authorization) = headers.authorization {
        let identity = match app.verifier.verify(Some(authorization)).await {
            Ok(identity) => identity,
            Err(error @ AuthError::Forbidden(_)) => {
                return Err(Reply::error(403, error.to_string()))
            }
            Err(error) => return Err(Reply::error(401, error.to_string())),
        };
        return policy::ci_browser(&identity, repo).map_err(|message| Reply::error(403, message));
    }
    if !app.iap.enabled() {
        return Err(Reply::error(401, "send a bearer token"));
    }
    let viewer = match app.iap.verify(headers.iap_assertion).await {
        Ok(viewer) => viewer,
        Err(error @ ViewerError::Forbidden(_)) => return Err(Reply::error(403, error.to_string())),
        Err(error) => return Err(Reply::error(401, error.to_string())),
    };
    policy::iap_browser(&app.iap, &viewer, repo, &app.config.repositories)
        .map_err(|message| Reply::error(403, message))
}

/// The query's pairs, or the 400 to send.
fn params(headers: &Headers<'_>) -> Result<Vec<(String, String)>, Reply> {
    parse_query(headers.query).ok_or_else(|| Reply::error(400, "the query is not valid"))
}

fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// `GET /v1/cache/summary?repo=<owner/name>`
///
/// ```json
/// {"repo": "...", "manifests": 0, "manifest_bytes": 0, "blobs": 0, "blob_bytes": 0,
///  "last_write_at": null, "last_writer": null,
///  "lookups": [{"day": "YYYY-MM-DD", "hits": 0, "misses": 0}]}
/// ```
///
/// `manifests` and `manifest_bytes` are for `repo`. `blobs` and `blob_bytes`
/// are global: blobs are content-addressed and shared by every repository,
/// so no blob belongs to one. `lookups` are the last 14 UTC days that had a
/// lookup, newest first. `last_writer` is the writer of the newest manifest.
pub async fn summary(app: &App, headers: &Headers<'_>) -> Reply {
    let pairs = match params(headers) {
        Ok(pairs) => pairs,
        Err(reply) => return reply,
    };
    let Some(repo) = param(&pairs, "repo").filter(|repo| !repo.is_empty()) else {
        return authorize_first_then(app, headers, None, "summary needs repo").await;
    };
    if let Err(reply) = authorize(app, headers, repo).await {
        return reply;
    }
    match read_summary(app, repo).await {
        Ok(body) => Reply::json(200, json!(body)),
        Err(error) => db_error(error),
    }
}

/// A request that names no repository still gets 401 before 400, so an
/// unauthenticated caller learns nothing about parameters.
async fn authorize_first_then(
    app: &App,
    headers: &Headers<'_>,
    repo: Option<&str>,
    message: &str,
) -> Reply {
    // With no repository there is nothing to allow-list; only identity is
    // checked, by browsing a repository no caller can hold.
    if let Err(reply) = authorize(app, headers, repo.unwrap_or("")).await {
        if reply.status == 401 {
            return reply;
        }
    }
    Reply::error(400, message)
}

pub async fn read_summary(app: &App, repo: &str) -> anyhow::Result<Summary> {
    let client = app.pool.get().await?;
    let totals = client
        .query_one(
            "SELECT count(*), COALESCE(sum(size_bytes), 0)::bigint, max(created_at) \
             FROM cache.manifests WHERE repo = $1",
            &[&repo],
        )
        .await?;
    let manifests: i64 = totals.get(0);
    let manifest_bytes: i64 = totals.get(1);
    let last_write_at: Option<DateTime<Utc>> = totals.get(2);
    let blobs = client
        .query_one(
            "SELECT count(*), COALESCE(sum(size_bytes), 0)::bigint FROM cache.blobs",
            &[],
        )
        .await?;
    let last_writer = client
        .query_opt(
            "SELECT writer_ref, writer_run_id, writer_sha, writer_workflow \
             FROM cache.manifests WHERE repo = $1 \
             ORDER BY created_at DESC, key LIMIT 1",
            &[&repo],
        )
        .await?
        .map(|row| WriterOut {
            git_ref: row.get(0),
            run_id: row.get(1),
            sha: row.get(2),
            workflow: row.get(3),
        });
    let lookups = client
        .query(
            "SELECT day, hits, misses FROM cache.lookup_stats \
             WHERE repo = $1 AND day > (now() AT TIME ZONE 'UTC')::date - $2::int \
             ORDER BY day DESC",
            &[&repo, &(LOOKUP_DAYS as i32)],
        )
        .await?
        .iter()
        .map(|row| {
            let day: NaiveDate = row.get(0);
            let hits: i64 = row.get(1);
            let misses: i64 = row.get(2);
            LookupDay {
                day: day.to_string(),
                hits,
                misses,
            }
        })
        .collect::<Vec<_>>();
    Ok(Summary {
        repo: repo.to_string(),
        manifests,
        manifest_bytes,
        blobs: blobs.get(0),
        blob_bytes: blobs.get(1),
        last_write_at: last_write_at.map(rfc3339),
        last_writer,
        lookups,
    })
}

/// `GET /v1/cache/entries?repo=&limit=&before=&target=`
///
/// A page of manifests, newest `created_at` first, then by key. `next` is the
/// `before` for the following page, or null at the end. An undecodable
/// manifest appears with an empty `target` and zeroed fields.
pub async fn entries(app: &App, headers: &Headers<'_>) -> Reply {
    let pairs = match params(headers) {
        Ok(pairs) => pairs,
        Err(reply) => return reply,
    };
    let Some(repo) = param(&pairs, "repo").filter(|repo| !repo.is_empty()) else {
        return authorize_first_then(app, headers, None, "entries needs repo").await;
    };
    if let Err(reply) = authorize(app, headers, repo).await {
        return reply;
    }
    let limit = match param(&pairs, "limit") {
        None => DEFAULT_LIMIT,
        Some(text) => match text.parse::<usize>() {
            Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
            _ => return Reply::error(400, format!("limit must be 1 to {MAX_LIMIT}")),
        },
    };
    let after = match param(&pairs, "before").filter(|text| !text.is_empty()) {
        None => None,
        Some(text) => match Cursor::parse(text) {
            Some(cursor) => Some(cursor),
            None => return Reply::error(400, "before is not a cursor this server issued"),
        },
    };
    let target = param(&pairs, "target").filter(|target| !target.is_empty());
    match read_entries(app, repo, limit, after, target).await {
        Ok((entries, next)) => Reply::json(200, json!({"entries": entries, "next": next})),
        Err(error) => db_error(error),
    }
}

struct Row {
    entry: Entry,
    created_at: DateTime<Utc>,
}

/// A page of at most `limit` entries after `after`, and the cursor for the
/// next page. `target` is a case-insensitive substring filter.
pub async fn read_entries(
    app: &App,
    repo: &str,
    limit: usize,
    after: Option<Cursor>,
    target: Option<&str>,
) -> anyhow::Result<(Vec<Entry>, Option<String>)> {
    let client = app.pool.get().await?;
    let needle = target.map(str::to_lowercase);
    let batch = if needle.is_some() {
        SCAN_BATCH
    } else {
        limit as i64 + 1
    };
    let mut cursor = after;
    let mut out: Vec<Entry> = Vec::new();
    let mut last_returned: Option<Cursor> = None;
    let mut scanned = 0;
    loop {
        let rows = fetch_page(&client, repo, cursor.as_ref(), batch).await?;
        let exhausted = (rows.len() as i64) < batch;
        for row in rows {
            let here = Cursor::new(row.created_at, &row.entry.key);
            scanned += 1;
            let matches = needle
                .as_deref()
                .is_none_or(|needle| row.entry.target.to_lowercase().contains(needle));
            if matches {
                if out.len() == limit {
                    // One more match exists past the page.
                    return Ok((out, last_returned.map(|c| c.encode())));
                }
                last_returned = Some(here.clone());
                out.push(row.entry);
            }
            cursor = Some(here);
            if scanned >= SCAN_CAP {
                // Stop reading; `next` resumes after the last row read.
                return Ok((out, cursor.map(|c| c.encode())));
            }
        }
        if exhausted {
            return Ok((out, None));
        }
    }
}

async fn fetch_page(
    client: &deadpool_postgres::Client,
    repo: &str,
    after: Option<&Cursor>,
    batch: i64,
) -> anyhow::Result<Vec<Row>> {
    let (at, key) = match after {
        Some(cursor) => (cursor.created_at(), Some(cursor.k.as_str())),
        None => (None, None),
    };
    let rows = client
        .query(
            "SELECT key, manifest, size_bytes, created_at, last_used_at, hits, last_hit_at, \
                    writer_ref, writer_run_id, writer_sha, writer_workflow \
             FROM cache.manifests \
             WHERE repo = $1 \
               AND ($2::timestamptz IS NULL \
                    OR created_at < $2 OR (created_at = $2 AND key > $3)) \
             ORDER BY created_at DESC, key \
             LIMIT $4",
            &[&repo, &at, &key, &batch],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let manifest: Vec<u8> = row.get(1);
            let created_at: DateTime<Utc> = row.get(3);
            let last_used_at: DateTime<Utc> = row.get(4);
            let last_hit_at: Option<DateTime<Utc>> = row.get(6);
            let decoded = decode_manifest(&manifest);
            Row {
                created_at,
                entry: Entry {
                    key: row.get(0),
                    target: decoded.target,
                    size_bytes: row.get(2),
                    output_files: decoded.output_files,
                    exit_code: decoded.exit_code,
                    duration_ms: decoded.duration_ms,
                    aster_version: decoded.aster_version,
                    created_at: rfc3339(created_at),
                    last_used_at: rfc3339(last_used_at),
                    hits: row.get(5),
                    last_hit_at: last_hit_at.map(rfc3339),
                    writer: WriterOut {
                        git_ref: row.get(7),
                        run_id: row.get(8),
                        sha: row.get(9),
                        workflow: row.get(10),
                    },
                },
            }
        })
        .collect())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Decoded {
    target: String,
    output_files: u64,
    exit_code: i64,
    duration_ms: u64,
    aster_version: String,
}

/// The fields a page shows, read field by field so one odd value does not
/// blank the rest. Anything that is not a JSON object decodes to zeros.
fn decode_manifest(bytes: &[u8]) -> Decoded {
    let Ok(Value::Object(manifest)) = serde_json::from_slice::<Value>(bytes) else {
        return Decoded::default();
    };
    Decoded {
        target: manifest
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        output_files: manifest
            .get("outputs")
            .and_then(Value::as_array)
            .map_or(0, |outputs| outputs.len() as u64),
        exit_code: manifest
            .get("exit_code")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        duration_ms: manifest
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        aster_version: manifest
            .get("aster_version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let at = DateTime::from_timestamp_micros(1_700_000_000_123_456).unwrap();
        let cursor = Cursor::new(at, "key/with spaces+and=signs");
        let text = cursor.encode();
        assert!(!text.contains(['+', '/', '=']), "{text} is URL-safe");
        assert_eq!(Cursor::decode(&text), Some(cursor));
        assert_eq!(Cursor::decode("not a cursor"), None);
        assert_eq!(Cursor::decode(""), None);
        let wrong_shape = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"t\":\"x\"}");
        assert_eq!(Cursor::decode(&wrong_shape), None);
    }

    #[test]
    fn decodes_manifest_fields_and_zeroes_an_undecodable_one() {
        let full = serde_json::to_vec(&json!({
            "schema": 1, "key": "k", "target": "//a:b", "exit_code": -1,
            "duration_ms": 42, "aster_version": "0.22.0",
            "outputs": [{"path": "x"}, {"path": "y"}],
        }))
        .unwrap();
        assert_eq!(
            decode_manifest(&full),
            Decoded {
                target: "//a:b".into(),
                output_files: 2,
                exit_code: -1,
                duration_ms: 42,
                aster_version: "0.22.0".into(),
            }
        );
        assert_eq!(decode_manifest(b"\xff\xfe not json"), Decoded::default());
        assert_eq!(decode_manifest(b"[1,2]"), Decoded::default());
        // A wrong-typed field zeroes only itself.
        let odd = serde_json::to_vec(&json!({"target": "//a:b", "duration_ms": "slow"})).unwrap();
        assert_eq!(decode_manifest(&odd).target, "//a:b");
        assert_eq!(decode_manifest(&odd).duration_ms, 0);
    }
}
