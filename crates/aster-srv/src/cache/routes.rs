//! `/v1/cache` routes.
//!
//! Lookup answers many keys at once and never errors on a miss. A hit names a
//! blob only when that object exists in GCS. Commit refuses a manifest that
//! names a missing blob, and refuses to replace a key that is already stored.

use base64::Engine;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use super::browse;
use super::policy::{self, Caller, Reader, Writer};
use super::signing::Method;
use crate::auth::Principal;
use crate::App;
use crate::Reply;

const MAX_KEYS: usize = 256;
const MAX_DIGESTS: usize = 1024;
const SHA256_LEN: usize = 64;

#[derive(Debug, Deserialize)]
struct LookupRequest {
    repo: String,
    keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UploadRequest {
    repo: String,
    digests: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CommitRequest {
    repo: String,
    key: String,
    /// Base64 of the manifest bytes (`Manifest::encode`).
    manifest: String,
}

/// The headers a cache route needs.
pub struct Headers<'a> {
    pub authorization: Option<&'a str>,
    /// The raw query string, for the read-only browsing routes.
    pub query: Option<&'a str>,
}

/// Handle one `/v1/cache/...` request. The caller has already matched the path.
pub async fn handle(
    app: &App,
    method: &str,
    path: &str,
    headers: &Headers<'_>,
    body: Bytes,
) -> Reply {
    // Test-only stand-in for the signed GCS URL. Production URLs are
    // `https://storage.googleapis.com/...` and never hit this path. The
    // end-to-end test rewrites that host to this server and keeps the signed
    // path, so `PUT /<bucket>/cas/<sha256>` stores the bytes commit later hashes.
    // A GCS-backed server does not answer this path at all.
    if app.config.dev_blob_route && app.cache_store.is_memory() {
        if let Some(digest) = test_blob_digest(path) {
            return test_blob(app, method, digest, body).await;
        }
    }
    if method == "GET" {
        return match path {
            "/v1/cache/summary" => browse::summary(app, headers).await,
            "/v1/cache/entries" => browse::entries(app, headers).await,
            _ => Reply::error(405, "method not allowed"),
        };
    }
    if method != "POST" {
        return Reply::error(405, "method not allowed");
    }
    let request = match serde_json::from_slice::<Value>(&body) {
        Ok(request) => request,
        Err(_) => return Reply::error(400, "the body must be JSON"),
    };
    match path {
        "/v1/cache/lookup" => lookup(app, headers, request).await,
        "/v1/cache/upload" => upload(app, headers, request).await,
        "/v1/cache/commit" => commit(app, headers, request).await,
        _ => Reply::error(404, "not found"),
    }
}

/// The sha256 a rewritten signed URL names, if this path is one.
///
/// A signed URL is `https://storage.googleapis.com/<bucket>/cas/<sha256>`.
/// After the test rewrites the host, the path is `/<bucket>/cas/<sha256>`.
fn test_blob_digest(path: &str) -> Option<&str> {
    let mut parts = path.split('/');
    if parts.next() != Some("") {
        return None;
    }
    let bucket = parts.next()?;
    if bucket.is_empty() || parts.next() != Some("cas") {
        return None;
    }
    let digest = parts.next()?;
    if parts.next().is_some() || !is_sha256(digest) {
        return None;
    }
    Some(digest)
}

/// PUT or GET one blob on the in-memory store. Not authenticated: a signed
/// GCS URL is the credential in production, and this path exists only so a
/// test can stand in for that URL. A GCS-backed store refuses it.
async fn test_blob(app: &App, method: &str, digest: &str, body: Bytes) -> Reply {
    if !is_sha256(digest) {
        return Reply::error(400, "digest is not a sha256");
    }
    if !app.cache_store.is_memory() {
        return Reply::error(404, "not found");
    }
    match method {
        "PUT" => match app.cache_store.put_unrecorded(digest, body).await {
            Ok(()) => Reply::bytes(200, Bytes::new()),
            Err(error) => db_error(error),
        },
        "GET" => match app.cache_store.get(digest).await {
            Ok(Some(bytes)) => Reply::bytes(200, bytes),
            Ok(None) => Reply::error(404, "missing"),
            Err(error) => db_error(error),
        },
        _ => Reply::error(405, "method not allowed"),
    }
}

async fn lookup(app: &App, headers: &Headers<'_>, body: Value) -> Reply {
    let request: LookupRequest = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(_) => return Reply::error(400, "lookup needs repo and keys"),
    };
    if request.keys.len() > MAX_KEYS {
        return Reply::error(400, format!("lookup accepts at most {MAX_KEYS} keys"));
    }
    let reader = match reader_for(app, headers, &request.repo, false).await {
        Ok(reader) => reader,
        Err(reply) => return reply,
    };
    if let Err(reply) = same_repo(&reader, &request.repo) {
        return reply;
    }
    // Only a writer-eligible CI caller's lookups bump `hits`, `last_hit_at` and
    // `last_used_at` and so keep entries alive. A person, and a pull-request
    // or other non-push CI token (`Reader::may_write` is false, from
    // `policy::ci_reader`), must not be able to hold an entry in the cache by
    // asking for it. The same policy decides who may commit; it is not repeated.
    let counts = reader.caller == Caller::Ci && reader.may_write;
    // Aggregate lookup stats stay a hit-rate view over every CI lookup.
    let records_stats = reader.caller == Caller::Ci;
    match lookup_hits(app, &reader.repository, &request.keys, counts).await {
        Ok(hits) => {
            let requested = request
                .keys
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len();
            if records_stats {
                record_lookup(app, &reader.repository, hits.len(), requested);
            }
            Reply::json(200, json!({"hits": hits}))
        }
        Err(error) => db_error(error),
    }
}

/// How long the stats write may wait, lock waits included, before it gives up.
const STATS_TIMEOUT_MS: u32 = 3000;

/// Add one lookup request to today's (UTC) `cache.lookup_stats` row for the
/// repository: `found` hits and the rest of `requested` as misses. The
/// per-manifest hit counts were already bumped by the lookup's own UPDATE.
///
/// The write runs on its own task so it never delays or fails the reply. A
/// failure or timeout is logged and the counts for that request are lost.
fn record_lookup(app: &App, repo: &str, found: usize, requested: usize) {
    if requested == 0 {
        return;
    }
    let hits = found as i64;
    let misses = requested as i64 - hits;
    let pool = app.pool.clone();
    let repo = repo.to_string();
    tokio::spawn(async move {
        if let Err(error) = write_stats(&pool, &repo, hits, misses).await {
            crate::log::warn(
                "cache lookup stats not recorded",
                json!({"error": format!("{error:#}"), "repo": repo}),
            );
        }
    });
}

async fn write_stats(
    pool: &deadpool_postgres::Pool,
    repo: &str,
    hits: i64,
    misses: i64,
) -> anyhow::Result<()> {
    let mut client = pool.get().await?;
    let transaction = client.transaction().await?;
    // Local to this transaction, so the pooled connection is left clean.
    transaction
        .batch_execute(&format!("SET LOCAL statement_timeout = {STATS_TIMEOUT_MS}"))
        .await?;
    transaction
        .execute(
            "INSERT INTO cache.lookup_stats (repo, day, hits, misses) \
             VALUES ($1, (now() AT TIME ZONE 'UTC')::date, $2, $3) \
             ON CONFLICT (repo, day) DO UPDATE SET \
                 hits = cache.lookup_stats.hits + EXCLUDED.hits, \
                 misses = cache.lookup_stats.misses + EXCLUDED.misses",
            &[&repo, &hits, &misses],
        )
        .await?;
    transaction.commit().await?;
    Ok(())
}

/// Blobs one lookup response may name, across all its hits. An entry whose
/// blobs would not fit is answered as a miss, so the client rebuilds that
/// target instead of the server doing unbounded GCS and IAM work. An entry
/// that alone names more than this is never served from the cache. Entries are
/// admitted in the order the client asked for them.
pub const MAX_LOOKUP_BLOBS: usize = 4096;

/// Blobs resolved (HEAD and signature) at once for one lookup. The store
/// bounds the same work across all requests.
const LOOKUP_CONCURRENCY: usize = 16;

/// What resolving one digest gave.
enum Resolved {
    Url(String),
    /// Not in GCS: the blob is left out and the client uploads it again.
    Absent,
    /// GCS or signing failed or was busy: the entry is answered as a miss.
    /// A blob still unresolved when the lookup budget runs out is treated
    /// the same way.
    Failed,
}

async fn lookup_hits(
    app: &App,
    repo: &str,
    keys: &[String],
    counts: bool,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    use futures_util::stream::{self, StreamExt};
    let client = app.pool.get().await?;
    let rows = client
        .query(
            "SELECT key, manifest FROM cache.manifests WHERE repo = $1 AND key = ANY($2)",
            &[&repo, &keys],
        )
        .await?;
    let mut found: std::collections::HashMap<String, Vec<u8>> = rows
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, Vec<u8>>(1)))
        .collect();

    // Admit entries in request order until the blob budget is spent.
    let mut admitted: Vec<(String, Vec<u8>, Vec<String>)> = Vec::new();
    let mut digests: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for key in keys {
        let Some(manifest) = found.remove(key) else {
            continue;
        };
        let refs = blob_refs(&manifest);
        let new = refs.iter().filter(|digest| !seen.contains(*digest)).count();
        if digests.len() + new > MAX_LOOKUP_BLOBS {
            crate::log::warn(
                "cache lookup over the blob cap; answered as a miss",
                json!({"repo": repo, "key": key, "blobs": refs.len(), "cap": MAX_LOOKUP_BLOBS}),
            );
            continue;
        }
        for digest in &refs {
            if seen.insert(digest.clone()) {
                digests.push(digest.clone());
            }
        }
        admitted.push((key.clone(), manifest, refs));
    }

    // Resolve at most LOOKUP_CONCURRENCY blobs at once, and stop at the
    // store's lookup budget: a blob still unresolved then leaves its entry a
    // miss, so the reply beats the client's request timeout. Without a
    // bucket every blob resolves at once as absent and there is no budget:
    // a zero one would race the stream and drop entries at random.
    let budget = app
        .cache_store
        .lookup_budget()
        .unwrap_or(std::time::Duration::MAX);
    let wanted = digests.len();
    let mut pending = stream::iter(digests)
        .map(|digest| async move {
            let result = match app.cache_store.readable_url(&digest).await {
                Ok(Some(url)) => Resolved::Url(url),
                Ok(None) => Resolved::Absent,
                Err(error) => {
                    crate::log::warn(
                        "cache lookup could not resolve a blob",
                        json!({"error": format!("{error:#}"), "digest": digest}),
                    );
                    Resolved::Failed
                }
            };
            (digest, result)
        })
        .buffer_unordered(LOOKUP_CONCURRENCY);
    let mut resolved: std::collections::HashMap<String, Resolved> =
        std::collections::HashMap::with_capacity(wanted);
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            next = pending.next() => match next {
                Some((digest, result)) => {
                    resolved.insert(digest, result);
                }
                None => break,
            },
            () = &mut deadline => {
                crate::log::warn(
                    "cache lookup ran out of time; unresolved entries are misses",
                    json!({
                        "repo": repo,
                        "budget_ms": budget.as_millis() as u64,
                        "resolved": resolved.len(),
                        "blobs": wanted,
                    }),
                );
                break;
            }
        }
    }
    // Dropping the stream cancels the HEADs and signatures still in flight.
    drop(pending);

    let mut hits = serde_json::Map::new();
    let mut served_keys: Vec<String> = Vec::new();
    let mut served_blobs: Vec<String> = Vec::new();
    for (key, manifest, refs) in admitted {
        // A blob that failed, or was not resolved within the budget, makes
        // the whole entry a miss: the server cannot say whether it exists.
        if refs.iter().any(|digest| {
            !matches!(
                resolved.get(digest),
                Some(Resolved::Url(_) | Resolved::Absent)
            )
        }) {
            continue;
        }
        let mut blobs = serde_json::Map::new();
        for digest in refs {
            if let Some(Resolved::Url(url)) = resolved.get(&digest) {
                blobs.insert(digest.clone(), Value::String(url.clone()));
                served_blobs.push(digest);
            }
        }
        served_keys.push(key.clone());
        hits.insert(
            key,
            json!({
                "manifest": base64::engine::general_purpose::STANDARD.encode(manifest),
                "blobs": blobs,
            }),
        );
    }
    if counts && !served_keys.is_empty() {
        // Only what the response serves counts and is kept alive.
        client
            .execute(
                "UPDATE cache.manifests \
                 SET last_used_at = now(), hits = hits + 1, last_hit_at = now() \
                 WHERE repo = $1 AND key = ANY($2)",
                &[&repo, &served_keys],
            )
            .await?;
        served_blobs.sort();
        served_blobs.dedup();
        client
            .execute(
                "UPDATE cache.blobs SET last_used_at = now() WHERE sha256 = ANY($1)",
                &[&served_blobs],
            )
            .await?;
    }
    Ok(hits)
}

async fn upload(app: &App, headers: &Headers<'_>, body: Value) -> Reply {
    let request: UploadRequest = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(_) => return Reply::error(400, "upload needs repo and digests"),
    };
    if request.digests.len() > MAX_DIGESTS {
        return Reply::error(400, format!("upload accepts at most {MAX_DIGESTS} digests"));
    }
    let reader = match reader_for(app, headers, &request.repo, true).await {
        Ok(reader) => reader,
        Err(reply) => return reply,
    };
    if let Err(reply) = same_repo(&reader, &request.repo) {
        return reply;
    }
    if !app.cache_store.enabled() {
        return Reply::error(503, "no cache bucket is configured");
    }
    let mut missing = serde_json::Map::new();
    for digest in &request.digests {
        if !is_sha256(digest) {
            return Reply::error(400, format!("digest {digest} is not a sha256"));
        }
        let present = match blob_present(app, digest).await {
            Ok(present) => present,
            Err(error) => return db_error(error),
        };
        if present {
            continue;
        }
        let url = match app.cache_store.signed_url(digest, Method::Put).await {
            Ok(url) => url,
            Err(error) => return db_error(error),
        };
        missing.insert(digest.clone(), Value::String(url));
    }
    Reply::json(200, json!({"missing": missing}))
}

async fn commit(app: &App, headers: &Headers<'_>, body: Value) -> Reply {
    let request: CommitRequest = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(_) => return Reply::error(400, "commit needs repo, key and manifest"),
    };
    if request.key.is_empty() || request.key.len() > 256 {
        return Reply::error(400, "the key must be 1 to 256 characters");
    }
    let reader = match reader_for(app, headers, &request.repo, true).await {
        Ok(reader) => reader,
        Err(reply) => return reply,
    };
    if let Err(reply) = same_repo(&reader, &request.repo) {
        return reply;
    }
    let manifest = match base64::engine::general_purpose::STANDARD.decode(request.manifest.trim()) {
        Ok(manifest) => manifest,
        Err(_) => return Reply::error(400, "manifest must be base64"),
    };
    if manifest.len() > app.config.max_body_bytes {
        return Reply::error(413, "the manifest is too large");
    }
    if !manifest_decodes(&manifest) {
        return Reply::error(400, "the manifest is not valid");
    }
    let digests = blob_refs(&manifest);
    match commit_manifest(
        app,
        &reader.repository,
        &request.key,
        &manifest,
        &digests,
        &reader.writer,
    )
    .await
    {
        Ok(Committed::Stored) => Reply::json(201, json!({"stored": true})),
        Ok(Committed::Exists) => Reply::error(409, "the key is already stored"),
        Ok(Committed::Missing(missing)) => Reply::json(
            422,
            json!({
                "error": "the manifest names blobs that are not in the cache",
                "missing": missing,
            }),
        ),
        Err(error) => db_error(error),
    }
}

enum Committed {
    Stored,
    Exists,
    /// Blobs that are absent, or whose bytes do not hash to their name.
    Missing(Vec<String>),
}

/// Store a manifest whose blobs are all present and verified.
///
/// The transaction holds the retention lock shared, so a retention pass
/// cannot delete a blob between this check and the insert that names it.
/// A blob with no `cache.blobs` row has only been PUT through a signed URL;
/// its bytes are hashed here before a row vouches for them, and an object
/// that does not match its name is deleted so the client can upload again.
async fn commit_manifest(
    app: &App,
    repo: &str,
    key: &str,
    manifest: &[u8],
    digests: &[String],
    writer: &Writer,
) -> anyhow::Result<Committed> {
    let summary = browse::ManifestColumns::of(manifest);
    let mut client = app.pool.get().await?;
    let transaction = client.transaction().await?;
    transaction
        .execute(
            "SELECT pg_advisory_xact_lock_shared($1)",
            &[&super::store::BLOB_LOCK],
        )
        .await?;
    let inserted = transaction
        .execute(
            "INSERT INTO cache.manifests \
                 (repo, key, manifest, size_bytes, \
                  writer_ref, writer_run_id, writer_sha, writer_workflow, \
                  target, output_files, exit_code, duration_ms, aster_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (repo, key) DO NOTHING",
            &[
                &repo,
                &key,
                &manifest,
                &(manifest.len() as i64),
                &writer.git_ref,
                &writer.run_id,
                &writer.sha,
                &writer.workflow,
                &summary.target,
                &summary.output_files,
                &summary.exit_code,
                &summary.duration_ms,
                &summary.aster_version,
            ],
        )
        .await?;
    if inserted == 0 {
        transaction.rollback().await?;
        return Ok(Committed::Exists);
    }
    let mut missing = Vec::new();
    for digest in digests {
        let recorded = transaction
            .query_opt("SELECT 1 FROM cache.blobs WHERE sha256 = $1", &[&digest])
            .await?
            .is_some();
        if recorded && app.cache_store.exists(digest).await? {
            transaction
                .execute(
                    "UPDATE cache.blobs SET last_used_at = now() WHERE sha256 = $1",
                    &[&digest],
                )
                .await?;
            continue;
        }
        match app.cache_store.hash(digest).await? {
            Some((actual, size)) if actual == *digest => {
                transaction
                    .execute(
                        "INSERT INTO cache.blobs (sha256, size_bytes) VALUES ($1, $2) \
                         ON CONFLICT (sha256) DO UPDATE \
                         SET size_bytes = EXCLUDED.size_bytes, last_used_at = now()",
                        &[&digest, &(size as i64)],
                    )
                    .await?;
            }
            Some(_) => {
                crate::log::error(
                    "cache blob does not match its digest",
                    json!({"sha256": digest, "repo": repo}),
                );
                app.cache_store.delete(digest).await?;
                missing.push(digest.clone());
            }
            None => missing.push(digest.clone()),
        }
    }
    if !missing.is_empty() {
        transaction.rollback().await?;
        // Forget rows for blobs that are gone or were deleted as corrupt, so
        // upload hands out a fresh PUT URL.
        let client = app.pool.get().await?;
        client
            .execute(
                "DELETE FROM cache.blobs WHERE sha256 = ANY($1)",
                &[&missing],
            )
            .await?;
        return Ok(Committed::Missing(missing));
    }
    transaction.commit().await?;
    Ok(Committed::Stored)
}

/// A blob is present when a commit has verified it (its row exists) and
/// the object is still in GCS. An unverified object gets a fresh PUT URL.
/// A row whose object is gone is dropped, so a later upload can replace it.
async fn blob_present(app: &App, digest: &str) -> anyhow::Result<bool> {
    let client = app.pool.get().await?;
    let recorded = client
        .query_opt("SELECT 1 FROM cache.blobs WHERE sha256 = $1", &[&digest])
        .await?
        .is_some();
    if !recorded {
        return Ok(false);
    }
    if app.cache_store.exists(digest).await? {
        return Ok(true);
    }
    client
        .execute("DELETE FROM cache.blobs WHERE sha256 = $1", &[&digest])
        .await?;
    Ok(false)
}

/// Digests a manifest names: each output `digest` and `log`.
pub fn blob_refs(manifest: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(manifest) else {
        return Vec::new();
    };
    if value.get("schema").and_then(Value::as_u64) != Some(1) {
        return Vec::new();
    }
    let mut refs = Vec::new();
    let mut push = |digest: &str| {
        if is_sha256(digest) && !refs.iter().any(|seen| seen == digest) {
            refs.push(digest.to_string());
        }
    };
    if let Some(outputs) = value.get("outputs").and_then(Value::as_array) {
        for output in outputs {
            if let Some(digest) = output.get("digest").and_then(Value::as_str) {
                push(digest);
            }
        }
    }
    if let Some(log) = value.get("log").and_then(Value::as_str) {
        push(log);
    }
    refs
}

fn manifest_decodes(manifest: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(manifest) else {
        return false;
    };
    value.get("schema").and_then(Value::as_u64) == Some(1)
        && value.get("key").and_then(Value::as_str).is_some()
        && value.get("outputs").and_then(Value::as_array).is_some()
}

fn is_sha256(value: &str) -> bool {
    value.len() == SHA256_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn same_repo(reader: &Reader, repo: &str) -> Result<(), Reply> {
    // A person names the repository in the body; a CI token may only touch
    // its own.
    if reader.caller == Caller::Ci && repo != reader.repository {
        return Err(Reply::error(403, "the token is for a different repository"));
    }
    Ok(())
}

async fn reader_for(
    app: &App,
    headers: &Headers<'_>,
    repo: &str,
    write: bool,
) -> Result<Reader, Reply> {
    // Every /v1 request carries its own bearer token. There is no fallback
    // to an IAP assertion: the route is served without IAP in front of it.
    let principal = app
        .principal(headers.authorization)
        .await
        .map_err(|error| error.into_reply())?;
    let reader = match principal {
        Principal::Ci(identity) => policy::ci_reader(&app.config, &identity),
        Principal::Person(person) => {
            policy::person_reader(&app.iap, &person, repo, &app.config.repositories)
                .map_err(|message| Reply::error(403, message))?
        }
    };
    if write {
        if let Err(error) = policy::require_writer(&reader) {
            return Err(Reply::error(403, error.to_string()));
        }
    }
    Ok(reader)
}

pub(super) fn db_error(error: anyhow::Error) -> Reply {
    crate::log::error(
        "cache request failed",
        json!({"error": format!("{error:#}")}),
    );
    Reply::error(500, "the cache is unavailable")
}
