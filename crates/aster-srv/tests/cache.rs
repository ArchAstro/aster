//! The shared artifact cache against real Postgres and an in-memory GCS.
//!
//! Set `ASTER_SRV_TEST_DATABASE_URL` to a Postgres URL whose user may create
//! databases. Without it the tests skip.

mod common;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aster_srv::cache::{expires_at, BlobStore, FixedSigner, Signer, URL_TTL};
use base64::Engine;
use chrono::{Duration, Utc};
use serde_json::{json, Value};

const REPO: &str = "ArchAstro/firstlanding";
const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const OTHER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn signer() -> Arc<FixedSigner> {
    Arc::new(FixedSigner {
        email: "cache-test@example.iam.gserviceaccount.com".into(),
        signature: b"test-signature".to_vec(),
        signed: Default::default(),
    })
}

fn manifest(key: &str, digests: &[&str]) -> String {
    let outputs: Vec<Value> = digests
        .iter()
        .enumerate()
        .map(|(index, digest)| {
            json!({
                "path": format!("out/{index}.bin"),
                "kind": "file",
                "digest": digest,
                "mode": 420,
            })
        })
        .collect();
    let bytes = serde_json::to_vec(&json!({
        "schema": 1,
        "key": key,
        "target": "//apps/web:build",
        "outputs": outputs,
    }))
    .unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn server() -> Option<(common::Server, Arc<FixedSigner>)> {
    let signing = signer();
    let store = BlobStore::memory("aster-cache-test", signing.clone() as Arc<dyn Signer>);
    let started = common::start_with(
        &[
            ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
            (
                "ASTER_SRV_CACHE_SIGNER",
                "cache-test@example.iam.gserviceaccount.com",
            ),
            ("ASTER_SRV_CACHE_DAYS", "14"),
            (
                "ASTER_SRV_IAP_AUDIENCE",
                "/projects/123456/global/backendServices/",
            ),
            ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
            (
                "ASTER_SRV_STATIC_TOKENS",
                "ArchAstro/firstlanding=dev-token-0123456789",
            ),
        ],
        Some(store),
    )
    .await?;
    Some((started, signing))
}

fn canonical_request_hash(url: &str, method: &str, object: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    let query = query
        .split('&')
        .filter(|pair| !pair.starts_with("X-Goog-Signature="))
        .collect::<Vec<_>>()
        .join("&");
    let encoded = object.replace('/', "%2F");
    let canonical = format!(
        "{method}\n/aster-cache-test/{encoded}\n{query}\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD"
    );
    use sha2::{Digest, Sha256};
    Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn bearer(repository: &str, workflow_ref: Option<&str>) -> String {
    format!(
        "Bearer {}",
        common::token_with_ref(repository, workflow_ref)
    )
}

async fn put_blob(server: &common::Server, digest: &str) {
    server
        .app
        .cache_store
        .put(&server.app.pool, digest, bytes::Bytes::from_static(b"blob"))
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_key_is_a_miss() {
    let Some((server, _)) = server().await else {
        return;
    };
    let (status, body) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["missing", "also-missing"]}),
            Some(&bearer(REPO, Some("refs/heads/main"))),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["hits"], json!({}));
    server.stop().await;
}

#[tokio::test]
async fn upload_commit_lookup_round_trip() {
    let Some((server, _)) = server().await else {
        return;
    };
    let writer = bearer(REPO, Some("refs/heads/main"));
    let (status, upload) = server
        .post_json(
            "/v1/cache/upload",
            &json!({"repo": REPO, "digests": [DIGEST, OTHER]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{upload}");
    assert!(upload["missing"][DIGEST]
        .as_str()
        .unwrap()
        .contains("X-Goog-Expires=900"));
    assert!(upload["missing"][OTHER].as_str().is_some());

    put_blob(&server, DIGEST).await;
    put_blob(&server, OTHER).await;
    let encoded = manifest("task-key", &[DIGEST]);
    let (status, committed) = server
        .post_json(
            "/v1/cache/commit",
            &json!({"repo": REPO, "key": "task-key", "manifest": encoded}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 201, "{committed}");

    let (status, again) = server
        .post_json(
            "/v1/cache/commit",
            &json!({"repo": REPO, "key": "task-key", "manifest": encoded}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 409, "{again}");

    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["task-key", "missing"]}),
            Some(&bearer(REPO, Some("refs/pull/9/merge"))),
            None,
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    assert!(looked["hits"].get("missing").is_none());
    let hit = &looked["hits"]["task-key"];
    assert_eq!(hit["manifest"], encoded);
    let url = hit["blobs"][DIGEST].as_str().expect("signed GET");
    assert!(url.contains(&format!("/cas/{DIGEST}")));
    assert!(url.starts_with("https://storage.googleapis.com/"));
    // The second digest was uploaded but the manifest does not name it.
    assert!(hit["blobs"].get(OTHER).is_none());
    server.stop().await;
}

#[tokio::test]
async fn commit_refuses_missing_blob() {
    let Some((server, _)) = server().await else {
        return;
    };
    let writer = bearer(REPO, Some("refs/heads/main"));
    let (status, body) = server
        .post_json(
            "/v1/cache/commit",
            &json!({
                "repo": REPO,
                "key": "needs-blob",
                "manifest": manifest("needs-blob", &[OTHER]),
            }),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["missing"], json!([OTHER]));

    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["needs-blob"]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    assert_eq!(looked["hits"], json!({}));
    server.stop().await;
}

#[tokio::test]
async fn commit_verifies_uploaded_bytes_against_their_digest() {
    let Some((server, _)) = server().await else {
        return;
    };
    let writer = bearer(REPO, Some("refs/heads/main"));
    let store = &server.app.cache_store;
    // A signed PUT that sent other bytes under DIGEST, the sha256 of "".
    store
        .put_unrecorded(DIGEST, bytes::Bytes::from_static(b"forged"))
        .await
        .unwrap();
    let (status, upload) = server
        .post_json(
            "/v1/cache/upload",
            &json!({"repo": REPO, "digests": [DIGEST]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{upload}");
    // Unverified bytes are not trusted, so the client may upload again.
    assert!(upload["missing"][DIGEST].as_str().is_some(), "{upload}");
    let commit = json!({
        "repo": REPO,
        "key": "verified",
        "manifest": manifest("verified", &[DIGEST]),
    });
    let (status, body) = server
        .post_json("/v1/cache/commit", &commit, Some(&writer), None)
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["missing"], json!([DIGEST]));
    assert!(
        store.objects().await.is_empty(),
        "the forged object is deleted"
    );

    // The right bytes commit, and the blob is recorded with its size.
    store
        .put_unrecorded(DIGEST, bytes::Bytes::new())
        .await
        .unwrap();
    let (status, body) = server
        .post_json("/v1/cache/commit", &commit, Some(&writer), None)
        .await;
    assert_eq!(status, 201, "{body}");
    let (status, upload) = server
        .post_json(
            "/v1/cache/upload",
            &json!({"repo": REPO, "digests": [DIGEST]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{upload}");
    assert_eq!(upload["missing"], json!({}));
    server.stop().await;
}

#[tokio::test]
async fn retention_waits_for_a_commit_in_progress() {
    let Some((server, _)) = server().await else {
        return;
    };
    // A commit holds the blob lock shared between checking its blobs and
    // inserting the manifest. Retention must not delete under it.
    let mut held = server.client().await;
    let transaction = held.transaction().await.unwrap();
    transaction
        .execute(
            "SELECT pg_advisory_xact_lock_shared($1)",
            &[&0x6173_7465_722d_6362_i64],
        )
        .await
        .unwrap();
    let app = Arc::clone(&server.app);
    let pass = tokio::spawn(async move { aster_srv::cache::run_retention(&app, Utc::now()).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!pass.is_finished(), "retention ran during a commit");
    transaction.rollback().await.unwrap();
    pass.await
        .unwrap()
        .unwrap()
        .expect("this replica holds the lock");
    server.stop().await;
}

#[tokio::test]
async fn lookup_omits_a_blob_missing_from_gcs() {
    let Some((server, _)) = server().await else {
        return;
    };
    let writer = bearer(REPO, Some("refs/heads/main"));
    put_blob(&server, DIGEST).await;
    put_blob(&server, OTHER).await;
    let (status, committed) = server
        .post_json(
            "/v1/cache/commit",
            &json!({
                "repo": REPO,
                "key": "partial",
                "manifest": manifest("partial", &[DIGEST, OTHER]),
            }),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 201, "{committed}");
    server.app.cache_store.delete(OTHER).await.unwrap();
    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["partial"]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    let blobs = &looked["hits"]["partial"]["blobs"];
    assert!(blobs.get(DIGEST).is_some(), "{looked}");
    assert!(blobs.get(OTHER).is_none(), "{looked}");
    server.stop().await;
}

#[tokio::test]
async fn policy_matrix_pr_oidc_forbidden_iap_reads_only() {
    let Some((server, _)) = server().await else {
        return;
    };
    let body = json!({"repo": REPO, "digests": [DIGEST]});
    let commit = json!({
        "repo": REPO,
        "key": "k",
        "manifest": manifest("k", &[]),
    });

    let (status, _) = server
        .post_json(
            "/v1/cache/upload",
            &body,
            Some(&bearer(REPO, Some("refs/pull/4/merge"))),
            None,
        )
        .await;
    assert_eq!(status, 403);
    let (status, _) = server
        .post_json(
            "/v1/cache/commit",
            &commit,
            Some(&bearer(REPO, Some("refs/pull/4/merge"))),
            None,
        )
        .await;
    assert_eq!(status, 403);

    // A token with no ref cannot write, and cannot be mistaken for main.
    let (status, _) = server
        .post_json("/v1/cache/upload", &body, Some(&bearer(REPO, None)), None)
        .await;
    assert_eq!(status, 403);

    let (status, _) = server
        .post_json(
            "/v1/cache/upload",
            &body,
            Some(&bearer(REPO, Some("refs/heads/main"))),
            None,
        )
        .await;
    assert_eq!(status, 200);

    // A static development token may write.
    let (status, _) = server
        .post_json(
            "/v1/cache/upload",
            &body,
            Some("Bearer dev-token-0123456789"),
            None,
        )
        .await;
    assert_eq!(status, 200);

    let viewer = common::iap_assertion("ada@example.com");
    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            None,
            Some(&viewer),
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    let (status, denied) = server
        .post_json("/v1/cache/upload", &body, None, Some(&viewer))
        .await;
    assert_eq!(status, 403, "{denied}");
    let (status, denied) = server
        .post_json("/v1/cache/commit", &commit, None, Some(&viewer))
        .await;
    assert_eq!(status, 403, "{denied}");

    let outsider = common::iap_assertion("ada@other.test");
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            None,
            Some(&outsider),
        )
        .await;
    assert_eq!(status, 403);

    // A viewer may not look up a repository the server does not serve.
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": "evil/other", "keys": ["k"]}),
            None,
            Some(&viewer),
        )
        .await;
    assert_eq!(status, 403);
    server.stop().await;
}

#[tokio::test]
async fn signed_urls_expire_and_no_key_is_stored() {
    let Some((server, signing)) = server().await else {
        return;
    };
    let (status, upload) = server
        .post_json(
            "/v1/cache/upload",
            &json!({"repo": REPO, "digests": [DIGEST]}),
            Some(&bearer(REPO, Some("refs/heads/main"))),
            None,
        )
        .await;
    assert_eq!(status, 200, "{upload}");
    let url = upload["missing"][DIGEST].as_str().unwrap();
    let expiry = expires_at(url).expect("the URL names when it expires");
    let now = SystemTime::now();
    let remaining = expiry.duration_since(UNIX_EPOCH).unwrap().as_secs()
        - now.duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!(remaining <= URL_TTL.as_secs(), "ttl {remaining}");
    assert!(remaining + 5 >= URL_TTL.as_secs(), "ttl {remaining}");
    assert!(url.contains("X-Goog-Algorithm=GOOG4-RSA-SHA256"));
    // The client sends only the URL, so the signature may cover no header
    // but `host`.
    assert!(url.contains("X-Goog-SignedHeaders=host&"), "{url}");
    assert!(url.contains("cache-test%40example.iam.gserviceaccount.com"));
    assert!(!url.contains("PRIVATE"));
    assert!(!url.to_ascii_lowercase().contains("begin private"));
    let payloads = signing.signed.lock().unwrap().clone();
    assert_eq!(payloads.len(), 1);
    let payload = String::from_utf8(payloads[0].clone()).unwrap();
    assert!(payload.starts_with("GOOG4-RSA-SHA256\n"), "{payload}");
    // The URL keeps the slash. GCS hashes each segment with `/` as `%2F`,
    // so the canonical request that was signed must encode it that way.
    assert!(url.contains(&format!("/aster-cache-test/cas/{DIGEST}?")));
    assert!(!url.contains("cas%2F"));
    let signed_hash = payload.lines().next_back().unwrap();
    let canonical = canonical_request_hash(url, "PUT", &format!("cas/{DIGEST}"));
    assert_eq!(signed_hash, canonical);

    // The process stores no service-account key file. The signer holds only
    // the email the URL names and the signature bytes it was given.
    assert!(!signing.email.contains("PRIVATE KEY"));
    server.stop().await;
}

#[tokio::test]
async fn retention_drops_expired_rows_and_objects() {
    let Some((server, _)) = server().await else {
        return;
    };
    let writer = bearer(REPO, Some("refs/heads/main"));
    put_blob(&server, DIGEST).await;
    put_blob(&server, OTHER).await;
    let (status, _) = server
        .post_json(
            "/v1/cache/commit",
            &json!({
                "repo": REPO,
                "key": "old",
                "manifest": manifest("old", &[DIGEST]),
            }),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 201);
    let (status, _) = server
        .post_json(
            "/v1/cache/commit",
            &json!({
                "repo": REPO,
                "key": "fresh",
                "manifest": manifest("fresh", &[OTHER]),
            }),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 201);

    let client = server.client().await;
    let stale = Utc::now() - Duration::days(15);
    client
        .execute(
            "UPDATE cache.manifests SET last_used_at = $1 WHERE key = 'old'",
            &[&stale],
        )
        .await
        .unwrap();
    // An object nothing names, plus the blob the expired manifest names.
    let orphan = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    put_blob(&server, orphan).await;
    client
        .execute(
            "UPDATE cache.blobs SET last_used_at = $2 WHERE sha256 = $1",
            &[&orphan, &stale],
        )
        .await
        .unwrap();

    // Signed PUTs no commit recorded: an old one is abandoned, a new one
    // may still be awaiting its commit.
    let abandoned = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let pending = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let store = &server.app.cache_store;
    store
        .put_unrecorded(abandoned, bytes::Bytes::from_static(b"blob"))
        .await
        .unwrap();
    store.backdate(abandoned, stale).await;
    store
        .put_unrecorded(pending, bytes::Bytes::from_static(b"blob"))
        .await
        .unwrap();

    let report = aster_srv::cache::run_retention(&server.app, Utc::now())
        .await
        .unwrap()
        .expect("this replica holds the lock");
    assert_eq!(report.manifests, 1);
    assert!(report
        .objects
        .iter()
        .any(|object| object == &format!("cas/{DIGEST}")));
    assert!(report
        .objects
        .iter()
        .any(|object| object == &format!("cas/{orphan}")));
    assert!(report
        .objects
        .iter()
        .any(|object| object == &format!("cas/{abandoned}")));
    assert!(!report.objects.iter().any(|object| object.contains(OTHER)));
    assert!(!report.objects.iter().any(|object| object.contains(pending)));

    let left: i64 = client
        .query_one("SELECT count(*) FROM cache.manifests", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 1);
    let blobs = server.app.cache_store.objects().await;
    assert_eq!(blobs, vec![OTHER.to_string(), pending.to_string()]);
    server.stop().await;
}
