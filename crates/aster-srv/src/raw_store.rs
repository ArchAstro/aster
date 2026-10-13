//! Keeping each upload exactly as it was sent, in Google Cloud Storage.
//!
//! The bucket's own lifecycle rule deletes objects after the retention
//! period. Credentials come from the GKE metadata server (Workload
//! Identity). Storing is best effort: the rows are already committed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::Mutex;

pub enum RawStore {
    /// Keep nothing.
    Disabled,
    Gcs {
        bucket: String,
        client: reqwest::Client,
        token: Mutex<Option<(String, Instant)>>,
    },
    /// Keep objects in memory. For tests.
    Memory(Mutex<Vec<(String, Bytes)>>),
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

const METADATA_TOKEN: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

impl RawStore {
    pub fn new(bucket: Option<String>) -> Self {
        match bucket {
            None => RawStore::Disabled,
            Some(bucket) => RawStore::Gcs {
                bucket,
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .expect("HTTP client builds"),
                token: Mutex::new(None),
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
                token,
            } => {
                let access = {
                    let mut cached = token.lock().await;
                    match cached.as_ref() {
                        Some((value, until)) if Instant::now() < *until => value.clone(),
                        _ => {
                            let fresh: MetadataToken = client
                                .get(METADATA_TOKEN)
                                .header("Metadata-Flavor", "Google")
                                .send()
                                .await?
                                .error_for_status()?
                                .json()
                                .await?;
                            let until = Instant::now()
                                + Duration::from_secs(fresh.expires_in.saturating_sub(60));
                            *cached = Some((fresh.access_token.clone(), until));
                            fresh.access_token
                        }
                    }
                };
                client
                    .post(format!(
                        "https://storage.googleapis.com/upload/storage/v1/b/{bucket}/o"
                    ))
                    .query(&[("uploadType", "media"), ("name", key)])
                    .bearer_auth(access)
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
