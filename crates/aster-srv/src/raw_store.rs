//! Keeping each upload exactly as it was sent, in Google Cloud Storage.
//!
//! The bucket's own lifecycle rule deletes objects after the retention
//! period. Credentials are Application Default Credentials: on GKE, the
//! pod's Workload Identity. Storing is best effort: the rows are already committed.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::OnceCell;

const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

pub enum RawStore {
    /// Keep nothing.
    Disabled,
    Gcs {
        bucket: String,
        client: reqwest::Client,
        /// Found on first use, so a server without Google credentials
        /// still starts.
        auth: OnceCell<Arc<dyn gcp_auth::TokenProvider>>,
    },
    /// Keep objects in memory. For tests.
    Memory(tokio::sync::Mutex<Vec<(String, Bytes)>>),
}

impl RawStore {
    pub fn new(bucket: Option<String>) -> Self {
        match bucket {
            None => RawStore::Disabled,
            Some(bucket) => RawStore::Gcs {
                bucket,
                client: crate::http_client()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .expect("HTTP client builds"),
                auth: OnceCell::new(),
            },
        }
    }

    /// Store `body` (gzip) under `key`.
    pub async fn put(&self, key: &str, body: Bytes) -> Result<()> {
        match self {
            RawStore::Disabled => Ok(()),
            RawStore::Memory(objects) => {
                objects.lock().await.push((key.to_string(), body));
                Ok(())
            }
            RawStore::Gcs {
                bucket,
                client,
                auth,
            } => {
                let provider = auth
                    .get_or_try_init(gcp_auth::provider)
                    .await
                    .context("no Google credentials")?;
                let access = provider
                    .token(&[SCOPE])
                    .await
                    .context("cannot get a Google access token")?;
                client
                    .post(format!(
                        "https://storage.googleapis.com/upload/storage/v1/b/{bucket}/o"
                    ))
                    .query(&[("uploadType", "media"), ("name", key)])
                    .bearer_auth(access.as_str())
                    .header("Content-Type", "application/gzip")
                    .body(body)
                    .send()
                    .await?
                    .error_for_status()
                    .context("the bucket refused the upload")?;
                Ok(())
            }
        }
    }
}
