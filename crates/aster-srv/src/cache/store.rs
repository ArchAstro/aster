//! Blobs in GCS, named in `cache.blobs`, and the retention pass that drops
//! what nothing still names.
//!
//! A blob is present only when the row exists and a HEAD of `cas/<sha256>`
//! succeeds. The row is written when a commit hashes the object and finds it
//! matches its name, so neither a signed PUT that never completed nor one
//! that sent other bytes can be committed or served.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, OnceCell};

use super::signing::{self, Method, Signer};

const STORAGE_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const RETENTION_LOCK: i64 = 0x6173_7465_722d_6363; // "aster-cc"
/// Commits hold this shared while they check blobs and insert the manifest
/// naming them; retention holds it exclusively while it decides what to
/// delete and deletes it. So a blob a commit has just checked cannot be
/// deleted before the manifest naming it is visible.
pub(crate) const BLOB_LOCK: i64 = 0x6173_7465_722d_6362; // "aster-cb"

/// What one retention pass removed. `None` from [`run_retention`] means
/// another replica held the lock.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub manifests: u64,
    pub blobs: u64,
    pub objects: Vec<String>,
}

/// Where blob bytes live.
pub enum BlobStore {
    /// No bucket is configured. Writes are refused; reads find nothing.
    Disabled,
    Gcs {
        bucket: String,
        client: reqwest::Client,
        auth: OnceCell<Arc<dyn gcp_auth::TokenProvider>>,
        signer_email: String,
        signer: OnceCell<Arc<dyn Signer>>,
    },
    /// Objects in memory, signed by `signer`. For tests.
    Memory {
        bucket: String,
        objects: Mutex<HashMap<String, (Bytes, DateTime<Utc>)>>,
        signer: Arc<dyn Signer>,
    },
}

impl BlobStore {
    /// `signer_email` is the service account that signs URLs. Config
    /// requires it whenever a bucket is set.
    pub fn new(bucket: Option<String>, signer_email: Option<String>) -> Self {
        match (bucket, signer_email) {
            (Some(bucket), Some(signer_email)) => BlobStore::Gcs {
                bucket,
                client: crate::http_client()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .expect("HTTP client builds"),
                auth: OnceCell::new(),
                signer_email,
                signer: OnceCell::new(),
            },
            _ => BlobStore::Disabled,
        }
    }

    /// An in-memory bucket. For tests.
    pub fn memory(bucket: impl Into<String>, signer: Arc<dyn Signer>) -> Self {
        BlobStore::Memory {
            bucket: bucket.into(),
            objects: Mutex::new(HashMap::new()),
            signer,
        }
    }

    /// Whether this is the in-memory store tests use in place of GCS.
    pub fn is_memory(&self) -> bool {
        matches!(self, BlobStore::Memory { .. })
    }

    /// Bytes stored as `cas/<sha256>`, or `None` when the object is absent.
    ///
    /// The in-memory store answers this directly. A GCS-backed store does
    /// not: production clients read through the signed URL, not this server.
    pub async fn get(&self, sha256: &str) -> Result<Option<Bytes>> {
        match self {
            BlobStore::Memory { objects, .. } => Ok(objects
                .lock()
                .await
                .get(sha256)
                .map(|(bytes, _)| bytes.clone())),
            BlobStore::Disabled | BlobStore::Gcs { .. } => Ok(None),
        }
    }

    /// Objects currently stored, by sha256. For tests.
    pub async fn objects(&self) -> Vec<String> {
        match self {
            BlobStore::Memory { objects, .. } => {
                let mut names: Vec<String> = objects.lock().await.keys().cloned().collect();
                names.sort();
                names
            }
            BlobStore::Disabled | BlobStore::Gcs { .. } => Vec::new(),
        }
    }

    /// Pretend `cas/<sha256>` was created at `at`. For tests.
    pub async fn backdate(&self, sha256: &str, at: DateTime<Utc>) {
        if let BlobStore::Memory { objects, .. } = self {
            if let Some(object) = objects.lock().await.get_mut(sha256) {
                object.1 = at;
            }
        }
    }

    /// Every `cas/<sha256>` object and when it was created, including
    /// signed PUTs no commit ever recorded.
    pub async fn list(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        match self {
            BlobStore::Disabled => Ok(Vec::new()),
            BlobStore::Memory { objects, .. } => Ok(objects
                .lock()
                .await
                .iter()
                .map(|(sha256, (_, created))| (sha256.clone(), *created))
                .collect()),
            BlobStore::Gcs { client, .. } => {
                #[derive(serde::Deserialize)]
                struct Page {
                    #[serde(default)]
                    items: Vec<Item>,
                    #[serde(rename = "nextPageToken")]
                    next_page_token: Option<String>,
                }
                #[derive(serde::Deserialize)]
                struct Item {
                    name: String,
                    #[serde(rename = "timeCreated")]
                    time_created: String,
                }
                let (bucket, token) = self.gcs_auth().await?;
                let url = format!("https://storage.googleapis.com/storage/v1/b/{bucket}/o");
                let mut listed = Vec::new();
                let mut page_token: Option<String> = None;
                loop {
                    let mut query = vec![
                        ("prefix", "cas/".to_string()),
                        (
                            "fields",
                            "items(name,timeCreated),nextPageToken".to_string(),
                        ),
                    ];
                    if let Some(page_token) = &page_token {
                        query.push(("pageToken", page_token.clone()));
                    }
                    let page: Page = client
                        .get(&url)
                        .bearer_auth(&token)
                        .query(&query)
                        .send()
                        .await
                        .context("cannot reach the bucket")?
                        .error_for_status()
                        .context("the bucket refused the listing")?
                        .json()
                        .await
                        .context("cannot read the bucket listing")?;
                    for item in page.items {
                        let Some(sha256) = item.name.strip_prefix("cas/") else {
                            continue;
                        };
                        let created = DateTime::parse_from_rfc3339(&item.time_created)
                            .with_context(|| format!("{} has no creation time", item.name))?
                            .with_timezone(&Utc);
                        listed.push((sha256.to_string(), created));
                    }
                    match page.next_page_token {
                        Some(next) => page_token = Some(next),
                        None => break,
                    }
                }
                Ok(listed)
            }
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, BlobStore::Disabled)
    }

    /// Whether `cas/<sha256>` exists.
    pub async fn exists(&self, sha256: &str) -> Result<bool> {
        match self {
            BlobStore::Disabled => Ok(false),
            BlobStore::Memory { objects, .. } => Ok(objects.lock().await.contains_key(sha256)),
            BlobStore::Gcs { client, .. } => {
                let (bucket, token) = self.gcs_auth().await?;
                let response = client
                    .head(object_url(&bucket, sha256))
                    .bearer_auth(token)
                    .send()
                    .await
                    .context("cannot reach the bucket")?;
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(false);
                }
                response
                    .error_for_status()
                    .context("the bucket refused the lookup")?;
                Ok(true)
            }
        }
    }

    /// The sha256 and length of `cas/<name>`'s bytes, or `None` when there
    /// is no such object.
    pub async fn hash(&self, name: &str) -> Result<Option<(String, u64)>> {
        use sha2::{Digest, Sha256};
        match self {
            BlobStore::Disabled => Ok(None),
            BlobStore::Memory { objects, .. } => Ok(objects
                .lock()
                .await
                .get(name)
                .map(|(bytes, _)| (signing::hex(&Sha256::digest(bytes)), bytes.len() as u64))),
            BlobStore::Gcs { client, .. } => {
                let (bucket, token) = self.gcs_auth().await?;
                let mut response = client
                    .get(object_url(&bucket, name))
                    .query(&[("alt", "media")])
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(600))
                    .send()
                    .await
                    .context("cannot reach the bucket")?;
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                response = response
                    .error_for_status()
                    .context("the bucket refused the download")?;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                while let Some(chunk) = response.chunk().await.context("cannot read the blob")? {
                    size += chunk.len() as u64;
                    hasher.update(&chunk);
                }
                Ok(Some((signing::hex(&hasher.finalize()), size)))
            }
        }
    }

    /// Store bytes under `cas/<sha256>` and record the blob, the way a
    /// client does after a signed PUT. Tests use this in place of uploading.
    pub async fn put(
        &self,
        pool: &deadpool_postgres::Pool,
        sha256: &str,
        body: Bytes,
    ) -> Result<()> {
        self.put_object(sha256, body.clone()).await?;
        let client = pool.get().await?;
        client
            .execute(
                "INSERT INTO cache.blobs (sha256, size_bytes) VALUES ($1, $2) \
                 ON CONFLICT (sha256) DO UPDATE SET size_bytes = EXCLUDED.size_bytes, last_used_at = now()",
                &[&sha256, &(body.len() as i64)],
            )
            .await?;
        Ok(())
    }

    /// Store the object without a `cache.blobs` row. For tests of a PUT
    /// the server has not been told about.
    pub async fn put_unrecorded(&self, sha256: &str, body: Bytes) -> Result<()> {
        self.put_object(sha256, body).await
    }

    async fn put_object(&self, sha256: &str, body: Bytes) -> Result<()> {
        match self {
            BlobStore::Disabled => anyhow::bail!("no cache bucket is configured"),
            BlobStore::Memory { objects, .. } => {
                objects
                    .lock()
                    .await
                    .insert(sha256.to_string(), (body, Utc::now()));
                Ok(())
            }
            BlobStore::Gcs { client, .. } => {
                let (bucket, token) = self.gcs_auth().await?;
                client
                    .put(object_url(&bucket, sha256))
                    .bearer_auth(token)
                    .header("Content-Type", "application/octet-stream")
                    .body(body)
                    .send()
                    .await?
                    .error_for_status()
                    .context("the bucket refused the upload")?;
                Ok(())
            }
        }
    }

    /// Delete `cas/<sha256>`. Missing is success.
    pub async fn delete(&self, sha256: &str) -> Result<()> {
        match self {
            BlobStore::Disabled => Ok(()),
            BlobStore::Memory { objects, .. } => {
                objects.lock().await.remove(sha256);
                Ok(())
            }
            BlobStore::Gcs { client, .. } => {
                let (bucket, token) = self.gcs_auth().await?;
                let response = client
                    .delete(object_url(&bucket, sha256))
                    .bearer_auth(token)
                    .send()
                    .await
                    .context("cannot reach the bucket")?;
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(());
                }
                response
                    .error_for_status()
                    .context("the bucket refused the delete")?;
                Ok(())
            }
        }
    }

    /// A signed URL for `cas/<sha256>`, valid for [`signing::URL_TTL`].
    pub async fn signed_url(&self, sha256: &str, method: Method) -> Result<String> {
        let (bucket, signer) = self.signer().await?;
        signing::signed_url(
            signer.as_ref(),
            &bucket,
            &object_name(sha256),
            method,
            SystemTime::now(),
            signing::URL_TTL,
        )
        .await
    }

    async fn signer(&self) -> Result<(String, Arc<dyn Signer>)> {
        match self {
            BlobStore::Disabled => anyhow::bail!("no cache bucket is configured"),
            BlobStore::Memory { bucket, signer, .. } => Ok((bucket.clone(), Arc::clone(signer))),
            BlobStore::Gcs {
                bucket,
                signer,
                signer_email,
                ..
            } => {
                let signer = signer
                    .get_or_try_init(|| async {
                        signing::IamSigner::new(signer_email.clone())
                            .await
                            .map(|signer| Arc::new(signer) as Arc<dyn Signer>)
                    })
                    .await?
                    .clone();
                Ok((bucket.clone(), signer))
            }
        }
    }

    async fn gcs_auth(&self) -> Result<(String, String)> {
        let BlobStore::Gcs { bucket, auth, .. } = self else {
            anyhow::bail!("no cache bucket is configured");
        };
        let provider = auth
            .get_or_try_init(gcp_auth::provider)
            .await
            .context("no Google credentials")?;
        let access = provider
            .token(&[STORAGE_SCOPE])
            .await
            .context("cannot get a Google access token")?;
        Ok((bucket.clone(), access.as_str().to_string()))
    }
}

fn object_name(sha256: &str) -> String {
    format!("cas/{sha256}")
}

fn object_url(bucket: &str, sha256: &str) -> String {
    format!(
        "https://storage.googleapis.com/{bucket}/{}",
        object_name(sha256)
    )
}

/// Drop manifests unused for `cache_days`, then blobs no remaining manifest
/// names, and delete those objects. Objects with no row, from signed PUTs
/// never committed, go too once they are older than the same cutoff, so an
/// upload awaiting its commit is left alone. Safe to run again; replicas take turns
/// through an advisory lock.
pub async fn run_retention(app: &crate::App, now: DateTime<Utc>) -> Result<Option<Report>> {
    let mut client = app.pool.get().await?;
    let transaction = client.transaction().await?;
    let locked: bool = transaction
        .query_one("SELECT pg_try_advisory_xact_lock($1)", &[&RETENTION_LOCK])
        .await?
        .get(0);
    if !locked {
        return Ok(None);
    }
    let cutoff = now - chrono::Duration::days(i64::from(app.config.cache_days));
    // List first, without blocking commits. An object created after this
    // listing is not a candidate this pass.
    let listed = app.cache_store.list().await?;
    transaction
        .execute("SELECT pg_advisory_xact_lock($1)", &[&BLOB_LOCK])
        .await?;
    let manifests = transaction
        .execute(
            "DELETE FROM cache.manifests WHERE last_used_at < $1",
            &[&cutoff],
        )
        .await?;
    // Lookup counts only feed a 14-day view; keep a month, or the cache's own
    // retention if that is longer, so the table does not grow without bound.
    let stats_days = i64::from(app.config.cache_days.max(30));
    let stats_cutoff = (now - chrono::Duration::days(stats_days)).date_naive();
    transaction
        .execute(
            "DELETE FROM cache.lookup_stats WHERE day < $1",
            &[&stats_cutoff],
        )
        .await?;
    let named = transaction
        .query("SELECT manifest FROM cache.manifests", &[])
        .await?;
    let mut keep = HashSet::new();
    for row in named {
        let bytes: Vec<u8> = row.get(0);
        keep.extend(super::routes::blob_refs(&bytes));
    }
    let blobs = transaction
        .query("SELECT sha256 FROM cache.blobs", &[])
        .await?;
    let mut orphans = Vec::new();
    for row in blobs {
        let sha256: String = row.get(0);
        if !keep.contains(&sha256) {
            orphans.push(sha256);
        }
    }
    let rows = orphans.len() as u64;
    let mut targets: HashSet<String> = orphans.iter().cloned().collect();
    for (sha256, created) in listed {
        if created < cutoff && !keep.contains(&sha256) {
            targets.insert(sha256);
        }
    }
    if !orphans.is_empty() {
        transaction
            .execute(
                "DELETE FROM cache.blobs WHERE sha256 = ANY($1)",
                &[&orphans],
            )
            .await?;
    }
    // Delete the objects before the rows commit. A failed delete rolls the
    // rows back, so the next pass tries them again instead of leaving an
    // object nothing names.
    let mut objects = Vec::new();
    for sha256 in &targets {
        app.cache_store.delete(sha256).await?;
        objects.push(object_name(sha256));
    }
    transaction.commit().await?;
    objects.sort();
    Ok(Some(Report {
        manifests,
        blobs: rows,
        objects,
    }))
}

/// Run a pass now and every six hours after. The pass also drops sign-in
/// codes nobody redeemed (`auth.cli_codes`), which would otherwise wait for
/// the next sign-in to clear them.
pub fn spawn(app: Arc<crate::App>) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = crate::cli_auth::purge_expired(&app).await {
                crate::log::warn(
                    "sign-in code purge failed",
                    serde_json::json!({"error": format!("{error:#}")}),
                );
            }
            match run_retention(&app, Utc::now()).await {
                Ok(Some(report)) => crate::log::info(
                    "cache retention pass",
                    serde_json::json!({
                        "manifests": report.manifests,
                        "blobs": report.blobs,
                        "objects": report.objects,
                    }),
                ),
                Ok(None) => {}
                Err(error) => crate::log::error(
                    "cache retention pass failed",
                    serde_json::json!({"error": format!("{error:#}")}),
                ),
            }
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });
}
