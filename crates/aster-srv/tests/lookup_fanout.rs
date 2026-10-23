//! N5: a lookup's GCS and IAM work is bounded.
//!
//! Real Postgres and the real routes, an in-memory GCS with injected HEAD
//! latency and a signer with injected `signBlob` latency. Set
//! `ASTER_SRV_TEST_DATABASE_URL`; without it the tests skip.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster_srv::cache::{BlobStore, FanoutConfig, SignLimits, Signer, MAX_LOOKUP_BLOBS};
use serde_json::{json, Value};

const REPO: &str = "ArchAstro/firstlanding";

/// A signer that takes `delay` per call and counts calls.
struct SlowSigner {
    delay: Duration,
    calls: AtomicUsize,
}

impl Signer for SlowSigner {
    fn sign<'a>(
        &'a self,
        _payload: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<u8>>> + Send + 'a>>
    {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(b"sig".to_vec())
        })
    }

    fn service_account_email(&self) -> &str {
        "cache-test@example.iam.gserviceaccount.com"
    }
}

fn digest(index: usize) -> String {
    format!("{index:064x}")
}

async fn server(
    config: FanoutConfig,
    head_latency: Duration,
    sign_delay: Duration,
) -> Option<(common::Server, Arc<SlowSigner>)> {
    let signer = Arc::new(SlowSigner {
        delay: sign_delay,
        calls: AtomicUsize::new(0),
    });
    let store = BlobStore::memory_with(
        "aster-cache-test",
        signer.clone() as Arc<dyn Signer>,
        config,
        head_latency,
    );
    let started = common::start_with(
        &[
            ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
            (
                "ASTER_SRV_CACHE_SIGNER",
                "cache-test@example.iam.gserviceaccount.com",
            ),
            ("ASTER_SRV_CACHE_DAYS", "14"),
        ],
        Some(store),
    )
    .await?;
    Some((started, signer))
}

/// Entry `key` naming `blobs` (one per output), stored as if CI had committed it.
async fn seed(server: &common::Server, key: &str, blobs: &[String]) {
    let outputs: Vec<Value> = blobs
        .iter()
        .enumerate()
        .map(|(index, digest)| {
            json!({"path": format!("out/{index}"), "kind": "file", "digest": digest, "mode": 420})
        })
        .collect();
    let manifest = serde_json::to_vec(&json!({
        "schema": 1, "key": key, "target": "//a:b", "outputs": outputs,
    }))
    .unwrap();
    let client = server.client().await;
    client
        .execute(
            "INSERT INTO cache.manifests (repo, key, manifest, size_bytes, target) \
             VALUES ($1, $2, $3, $4, '//a:b')",
            &[&REPO, &key, &manifest, &(manifest.len() as i64)],
        )
        .await
        .unwrap();
}

async fn put(server: &common::Server, blobs: &[String]) {
    for digest in blobs {
        server
            .app
            .cache_store
            .put(&server.app.pool, digest, bytes::Bytes::from_static(b"b"))
            .await
            .unwrap();
    }
}

fn main_push() -> String {
    format!(
        "Bearer {}",
        common::token_with_ref(REPO, Some("refs/heads/main"))
    )
}

async fn lookup(server: &common::Server, keys: &[String]) -> (u16, Value, Duration) {
    let started = Instant::now();
    let (status, body) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": keys}),
            Some(&main_push()),
            None,
        )
        .await;
    (status, body, started.elapsed())
}

/// The latency story behind the observed remote-cache read misses: 64 keys of
/// 8 blobs each (512 blobs), 20ms per HEAD and 10ms per signBlob. One at a
/// time that is 512 * 30ms = 15.4s (17.8s measured before the fix), past the 0.22 client's 5s timeout. With
/// bounded concurrency a cold lookup takes about a second, and a repeat costs
/// no GCS or IAM call at all.
#[tokio::test]
async fn a_wide_lookup_finishes_in_bounded_time_and_a_repeat_is_free() {
    let Some((server, signer)) = server(
        FanoutConfig::default(),
        Duration::from_millis(20),
        Duration::from_millis(10),
    )
    .await
    else {
        return;
    };
    // Phase 1: 64 entries, 8 distinct blobs each, all present in GCS.
    let keys: Vec<String> = (0..64).map(|index| format!("k{index}")).collect();
    for (index, key) in keys.iter().enumerate() {
        let blobs: Vec<String> = (0..8).map(|blob| digest(index * 8 + blob)).collect();
        put(&server, &blobs).await;
        seed(&server, key, &blobs).await;
    }

    // Phase 2: a cold lookup resolves all 512 blobs concurrently.
    let (status, body, cold) = lookup(&server, &keys).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["hits"].as_object().unwrap().len(), 64);
    assert!(body["hits"]["k63"]["blobs"][digest(511)]
        .as_str()
        .unwrap()
        .starts_with("https://storage.googleapis.com/aster-cache-test/cas/"));
    assert_eq!(server.app.cache_store.head_calls(), 512);
    assert_eq!(signer.calls.load(Ordering::SeqCst), 512);

    // Phase 3: the same lookup again is served from memory.
    let (status, again, warm) = lookup(&server, &keys).await;
    assert_eq!(status, 200);
    assert_eq!(again["hits"], body["hits"]);
    assert_eq!(server.app.cache_store.head_calls(), 512);
    assert_eq!(signer.calls.load(Ordering::SeqCst), 512);

    eprintln!(
        "lookup_fanout: 64 keys x 8 blobs, HEAD 20ms, sign 10ms: cold {cold:?}, warm {warm:?} (before the fix, measured: 17.8s cold, 17.9s warm)"
    );
    assert!(cold < Duration::from_secs(4), "cold lookup took {cold:?}");
    assert!(
        warm < Duration::from_millis(500),
        "warm lookup took {warm:?}"
    );
    server.stop().await;
}

/// Entries past the per-response blob cap are answered as misses, in request
/// order, and only the entries served are counted.
#[tokio::test]
async fn entries_over_the_blob_cap_are_misses_and_are_not_counted() {
    let Some((server, _)) = server(FanoutConfig::default(), Duration::ZERO, Duration::ZERO).await
    else {
        return;
    };
    // Five entries of 1000 distinct blobs: the first four (4000) fit in 4096.
    let per_entry = MAX_LOOKUP_BLOBS / 4 - 24;
    let keys: Vec<String> = (0..5).map(|index| format!("big{index}")).collect();
    for (index, key) in keys.iter().enumerate() {
        let blobs: Vec<String> = (0..per_entry).map(|n| digest(index * 10_000 + n)).collect();
        for digest in &blobs {
            let bytes = bytes::Bytes::from_static(b"b");
            server
                .app
                .cache_store
                .put_unrecorded(digest, bytes)
                .await
                .unwrap();
        }
        seed(&server, key, &blobs).await;
    }
    let (status, body, _) = lookup(&server, &keys).await;
    assert_eq!(status, 200);
    let hits = body["hits"].as_object().unwrap();
    assert_eq!(hits.len(), 4, "{:?}", hits.keys().collect::<Vec<_>>());
    assert!(hits.get("big4").is_none());
    let counted: Vec<(String, i64)> = server
        .client()
        .await
        .query("SELECT key, hits FROM cache.manifests ORDER BY key", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(counted.last().unwrap(), &("big4".to_string(), 0));
    assert!(counted[..4].iter().all(|(_, hits)| *hits == 1));
    server.stop().await;
}

/// When IAM signing is saturated, entries that cannot be signed become
/// misses, the lookup still answers 200, and nothing hammers IAM.
#[tokio::test]
async fn a_saturated_signer_turns_entries_into_misses_not_errors() {
    let config = FanoutConfig {
        sign: SignLimits {
            concurrency: 4,
            per_second: 0.01,
            burst: 3.0,
            max_wait: Duration::from_millis(20),
        },
        ..FanoutConfig::default()
    };
    let Some((server, signer)) = server(config, Duration::ZERO, Duration::ZERO).await else {
        return;
    };
    let keys: Vec<String> = (0..6).map(|index| format!("s{index}")).collect();
    for (index, key) in keys.iter().enumerate() {
        let blobs = vec![digest(index)];
        put(&server, &blobs).await;
        seed(&server, key, &blobs).await;
    }
    let (status, body, _) = lookup(&server, &keys).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["hits"].as_object().unwrap().len(), 3, "{body}");
    assert_eq!(signer.calls.load(Ordering::SeqCst), 3);
    // Only the three served entries were counted.
    let counted: i64 = server
        .client()
        .await
        .query_one("SELECT sum(hits)::bigint FROM cache.manifests", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(counted, 3);
    server.stop().await;
}

/// A blob that disappears from GCS is noticed once the remembered answer
/// expires, and a lookup then leaves it out.
#[tokio::test]
async fn a_blob_deleted_from_gcs_drops_out_after_the_remembered_answer_expires() {
    let config = FanoutConfig {
        present_ttl: Duration::from_millis(50),
        ..FanoutConfig::default()
    };
    let Some((server, _)) = server(config, Duration::ZERO, Duration::ZERO).await else {
        return;
    };
    let blobs = vec![digest(1), digest(2)];
    put(&server, &blobs).await;
    seed(&server, "k", &blobs).await;
    let keys = vec!["k".to_string()];
    let (_, first, _) = lookup(&server, &keys).await;
    assert_eq!(first["hits"]["k"]["blobs"].as_object().unwrap().len(), 2);
    server.app.cache_store.delete(&blobs[1]).await.unwrap();
    // delete forgets the digest at once; a stale positive would need the TTL.
    let (_, second, _) = lookup(&server, &keys).await;
    assert_eq!(second["hits"]["k"]["blobs"].as_object().unwrap().len(), 1);
    server.stop().await;
}
