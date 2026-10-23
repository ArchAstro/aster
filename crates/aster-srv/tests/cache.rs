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
                "/projects/123456/global/backendServices/4242",
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
    let canonical = format!(
        "{method}\n/aster-cache-test/{object}\n{query}\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD"
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
async fn policy_matrix_pr_oidc_forbidden_person_reads_only() {
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

    let viewer = common::person_bearer(&server, "ada@example.com");
    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    let (status, denied) = server
        .post_json("/v1/cache/upload", &body, Some(&viewer), None)
        .await;
    assert_eq!(status, 403, "{denied}");
    let (status, denied) = server
        .post_json("/v1/cache/commit", &commit, Some(&viewer), None)
        .await;
    assert_eq!(status, 403, "{denied}");

    let outsider = common::person_bearer(&server, "ada@other.test");
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k"]}),
            Some(&outsider),
            None,
        )
        .await;
    assert_eq!(status, 403);

    // A person may not look up a repository the server does not serve.
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": "evil/other", "keys": ["k"]}),
            Some(&viewer),
            None,
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
    // The URL and the canonical request both keep `/` literal. This stand-in
    // check cannot prove GCS accepts it; the V4 vectors in `signing.rs` do.
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

const VIEWER: &str = "ada@example.com";

fn manifest_for(key: &str, target: &str, digests: &[&str]) -> (String, usize) {
    let outputs: Vec<Value> = digests
        .iter()
        .enumerate()
        .map(|(index, digest)| {
            json!({"path": format!("out/{index}.bin"), "kind": "file", "digest": digest, "mode": 420})
        })
        .collect();
    let bytes = serde_json::to_vec(&json!({
        "schema": 1,
        "key": key,
        "target": target,
        "exit_code": 0,
        "duration_ms": 1500,
        "aster_version": "0.22.0",
        "outputs": outputs,
    }))
    .unwrap();
    let size = bytes.len();
    (
        base64::engine::general_purpose::STANDARD.encode(bytes),
        size,
    )
}

fn ci_writer(run_id: &str, sha: &str) -> String {
    format!(
        "Bearer {}",
        common::token_with_claims(
            REPO,
            json!({
                "ref": "refs/heads/main",
                "run_id": run_id,
                "sha": sha,
                "workflow_ref": format!("{REPO}/.github/workflows/ci.yml@refs/heads/main"),
            }),
        )
    )
}

async fn commit_as(server: &common::Server, writer: &str, key: &str, target: &str) -> usize {
    let (manifest, size) = manifest_for(key, target, &[DIGEST]);
    let (status, body) = server
        .post_json(
            "/v1/cache/commit",
            &json!({"repo": REPO, "key": key, "manifest": manifest}),
            Some(writer),
            None,
        )
        .await;
    assert_eq!(status, 201, "{body}");
    size
}

/// An operator checks that the shared cache works: two CI runs on main write
/// entries, one lookup hits one key and misses another, and a domain viewer
/// reads the counts, sizes, writers and hit totals back over the JSON routes.
#[tokio::test]
async fn a_viewer_reads_back_what_ci_wrote_and_how_lookups_fared() {
    let Some((server, _)) = server().await else {
        return;
    };

    // Phase 1: two CI runs on main commit one manifest each. The blob is
    // already in the store, so commit verifies and records it.
    put_blob(&server, DIGEST).await;
    let first = ci_writer("1001", "aaaa111");
    let second = ci_writer("1002", "bbbb222");
    let size_one = commit_as(&server, &first, "key-one", "//apps/web:build").await;
    let size_two = commit_as(&server, &second, "key-two", "//apps/api:test").await;

    // Phase 2: a CI lookup of one stored key and one unknown key counts one
    // hit and one miss.
    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["key-one", "never-stored"]}),
            Some(&first),
            None,
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    assert_eq!(looked["hits"].as_object().unwrap().len(), 1);
    // The stats write runs after the reply.
    assert_eq!(wait_for_stats(&server.client().await, 1, 1).await, (1, 1));

    // Phase 3: a signed-in person inside the allowed domain reads with their token.
    let viewer = common::person_bearer(&server, VIEWER);
    let (status, summary) = server
        .get_json(
            &format!("/v1/cache/summary?repo={REPO}"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{summary}");
    assert_eq!(summary["repo"], REPO);
    assert_eq!(summary["manifests"], 2);
    assert_eq!(summary["manifest_bytes"], (size_one + size_two) as i64);
    assert_eq!(summary["blobs"], 1);
    assert_eq!(summary["blob_bytes"], 4);
    assert!(summary["last_write_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        summary["last_writer"],
        json!({
            "ref": "refs/heads/main",
            "run_id": "1002",
            "sha": "bbbb222",
            "workflow": format!("{REPO}/.github/workflows/ci.yml@refs/heads/main"),
        })
    );
    let today = Utc::now().date_naive().to_string();
    assert_eq!(
        summary["lookups"],
        json!([{"day": today, "hits": 1, "misses": 1}])
    );

    let (status, page) = server
        .get_json(
            &format!("/v1/cache/entries?repo={REPO}"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["next"], Value::Null);
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    // Newest write first.
    let (newest, oldest) = (&entries[0], &entries[1]);
    assert_eq!(newest["key"], "key-two");
    assert_eq!(newest["target"], "//apps/api:test");
    assert_eq!(newest["size_bytes"], size_two as i64);
    assert_eq!(newest["output_files"], 1);
    assert_eq!(newest["exit_code"], 0);
    assert_eq!(newest["duration_ms"], 1500);
    assert_eq!(newest["aster_version"], "0.22.0");
    assert_eq!(newest["hits"], 0);
    assert_eq!(newest["last_hit_at"], Value::Null);
    assert_eq!(newest["writer"]["run_id"], "1002");
    assert_eq!(oldest["key"], "key-one");
    assert_eq!(oldest["hits"], 1);
    assert!(oldest["last_hit_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(oldest["writer"]["sha"], "aaaa111");
    assert_eq!(oldest["writer"]["ref"], "refs/heads/main");

    // A viewer may browse but not write.
    let (manifest, _) = manifest_for("viewer-key", "//x:y", &[DIGEST]);
    let (status, _) = server
        .post_json(
            "/v1/cache/commit",
            &json!({"repo": REPO, "key": "viewer-key", "manifest": manifest}),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, 403);
    server.stop().await;
}

#[tokio::test]
async fn browsing_routes_authorize_viewers_and_ci() {
    let Some((server, _)) = server().await else {
        return;
    };
    let path = format!("/v1/cache/summary?repo={REPO}");
    let entries = format!("/v1/cache/entries?repo={REPO}");
    for path in [&path, &entries] {
        // No identity at all.
        let (status, _) = server.get_json(path, None, None).await;
        assert_eq!(status, 401, "{path}");
        // A domain viewer may read.
        let (status, body) = server
            .get_json(path, Some(&common::person_bearer(&server, VIEWER)), None)
            .await;
        assert_eq!(status, 200, "{body}");
        // An email outside the viewer domains.
        let (status, _) = server
            .get_json(
                path,
                Some(&common::person_bearer(&server, "eve@other.test")),
                None,
            )
            .await;
        assert_eq!(status, 403);
        // CI reads its own repository, from any ref.
        let (status, _) = server
            .get_json(path, Some(&bearer(REPO, Some("refs/pull/9/merge"))), None)
            .await;
        assert_eq!(status, 200);
        let (status, _) = server
            .get_json(path, Some(&bearer("ArchAstro/other", None)), None)
            .await;
        assert_eq!(status, 403);
        // A person may not browse a repository the server does not serve.
        let other = path.replace(REPO, "evil/other");
        let (status, _) = server
            .get_json(&other, Some(&common::person_bearer(&server, VIEWER)), None)
            .await;
        assert_eq!(status, 403);
        // Bad bearer token.
        let (status, _) = server.get_json(path, Some("Bearer nope"), None).await;
        assert_eq!(status, 401);
    }
    // No repo and no identity is still 401; with identity it is 400.
    let (status, _) = server.get_json("/v1/cache/summary", None, None).await;
    assert_eq!(status, 401);
    let (status, _) = server
        .get_json(
            "/v1/cache/entries",
            Some(&common::person_bearer(&server, VIEWER)),
            None,
        )
        .await;
    assert_eq!(status, 400);
    // The routes are read-only.
    let (status, _) = server
        .post_json(
            &path,
            &json!({}),
            Some(&bearer(REPO, Some("refs/heads/main"))),
            None,
        )
        .await;
    assert_eq!(status, 405);
    server.stop().await;
}

/// Entries page by an opaque cursor over `created_at DESC, key`, including
/// rows that share a timestamp, a target filter, a manifest that does not
/// decode, and a static-token writer with no claims.
#[tokio::test]
async fn entries_page_by_cursor_filter_by_target_and_survive_a_bad_manifest() {
    let Some((server, _)) = server().await else {
        return;
    };
    let viewer = common::person_bearer(&server, VIEWER);
    let client = server.client().await;
    // Five rows; b and c share a created_at to exercise the key tie-break.
    let base = Utc::now();
    for (key, offset, target, manifest) in [
        ("a", 5, "//x:one", b"{\"target\":\"//x:one\"}".to_vec()),
        ("b", 4, "//x:two", b"{\"target\":\"//x:two\"}".to_vec()),
        ("c", 4, "//y:three", b"{\"target\":\"//y:three\"}".to_vec()),
        // A manifest that does not decode keeps the column defaults.
        ("d", 3, "", b"\xff\xfe not json".to_vec()),
        ("e", 2, "//x:five", b"{\"target\":\"//x:five\"}".to_vec()),
    ] {
        client
            .execute(
                "INSERT INTO cache.manifests (repo, key, manifest, size_bytes, created_at, target) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &REPO,
                    &key,
                    &manifest,
                    &(manifest.len() as i64),
                    &(base - Duration::minutes(offset)),
                    &target,
                ],
            )
            .await
            .unwrap();
    }
    let mut seen = Vec::new();
    let mut before = String::new();
    loop {
        let (status, page) = server
            .get_json(
                &format!("/v1/cache/entries?repo={REPO}&limit=2&before={before}"),
                Some(&viewer),
                None,
            )
            .await;
        assert_eq!(status, 200, "{page}");
        for entry in page["entries"].as_array().unwrap() {
            seen.push(entry["key"].as_str().unwrap().to_string());
        }
        match page["next"].as_str() {
            Some(next) => before = next.to_string(),
            None => break,
        }
    }
    assert_eq!(seen, ["e", "d", "b", "c", "a"]);

    // The undecodable manifest is listed with zeroed fields.
    let (_, page) = server
        .get_json(
            &format!("/v1/cache/entries?repo={REPO}"),
            Some(&viewer),
            None,
        )
        .await;
    let bad = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "d")
        .unwrap();
    assert_eq!(bad["target"], "");
    assert_eq!(bad["output_files"], 0);
    assert_eq!(bad["duration_ms"], 0);
    assert_eq!(
        bad["writer"],
        json!({"ref": null, "run_id": null, "sha": null, "workflow": null})
    );

    // A target substring filter, paged.
    let (status, filtered) = server
        .get_json(
            &format!("/v1/cache/entries?repo={REPO}&target=X%3A&limit=1"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{filtered}");
    assert_eq!(filtered["entries"][0]["key"], "e");
    assert!(filtered["next"].is_string());
    let (_, filtered) = server
        .get_json(
            &format!("/v1/cache/entries?repo={REPO}&target=x:&limit=5"),
            Some(&viewer),
            None,
        )
        .await;
    let keys: Vec<&str> = filtered["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["e", "b", "a"]);
    assert_eq!(filtered["next"], Value::Null);

    // Bad parameters.
    for query in ["limit=0", "limit=201", "limit=x", "before=garbage"] {
        let (status, _) = server
            .get_json(
                &format!("/v1/cache/entries?repo={REPO}&{query}"),
                Some(&viewer),
                None,
            )
            .await;
        assert_eq!(status, 400, "{query}");
    }
    // Another repository's entries are not mixed in.
    let (_, empty) = server
        .get_json(
            "/v1/cache/entries?repo=ArchAstro/other",
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(empty["entries"], json!([]));
    server.stop().await;
}

/// The repository's lookup totals once they reach `(hits, misses)`, or what
/// they are after five seconds.
async fn wait_for_stats(client: &deadpool_postgres::Client, hits: i64, misses: i64) -> (i64, i64) {
    let mut seen = (0, 0);
    for _ in 0..100 {
        if let Some(row) = client
            .query_opt(
                "SELECT hits, misses FROM cache.lookup_stats WHERE repo = $1",
                &[&REPO],
            )
            .await
            .unwrap()
        {
            seen = (row.get(0), row.get(1));
        }
        if seen == (hits, misses) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    seen
}

/// Lookups add to today's row across requests, count each distinct key once,
/// and keep serving when the stats table cannot be written.
#[tokio::test]
async fn lookup_stats_accumulate_per_day_and_never_fail_a_lookup() {
    let Some((server, _)) = server().await else {
        return;
    };
    put_blob(&server, DIGEST).await;
    let writer = bearer(REPO, Some("refs/heads/main"));
    commit_as(&server, &writer, "k1", "//a:b").await;
    let lookup = |keys: Vec<&'static str>| {
        let writer = writer.clone();
        let server = &server;
        async move {
            server
                .post_json(
                    "/v1/cache/lookup",
                    &json!({"repo": REPO, "keys": keys}),
                    Some(&writer),
                    None,
                )
                .await
        }
    };
    // Duplicate keys in one request count once.
    assert_eq!(lookup(vec!["k1", "k1", "m1"]).await.0, 200);
    assert_eq!(lookup(vec!["k1", "m1", "m2"]).await.0, 200);
    // An empty lookup records nothing.
    assert_eq!(lookup(vec![]).await.0, 200);
    let client = server.client().await;
    // The stats write runs after the reply, so wait for it.
    assert_eq!(wait_for_stats(&client, 2, 3).await, (2, 3));
    let manifest = client
        .query_one(
            "SELECT hits, last_hit_at IS NOT NULL FROM cache.manifests WHERE key = 'k1'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(manifest.get::<_, i64>(0), 2);
    assert!(manifest.get::<_, bool>(1));

    // Recording is best effort: with the table gone the lookup still answers.
    client
        .batch_execute("DROP TABLE cache.lookup_stats")
        .await
        .unwrap();
    let (status, body) = lookup(vec!["k1"]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["hits"].as_object().unwrap().len(), 1);
    drop(client);
    server.stop().await;
}

/// A commit records the OIDC claims of its writer; a static token has none.
#[tokio::test]
async fn commit_records_the_writer_claims() {
    let Some((server, _)) = server().await else {
        return;
    };
    put_blob(&server, DIGEST).await;
    commit_as(&server, &ci_writer("77", "cafe"), "from-ci", "//a:b").await;
    commit_as(
        &server,
        "Bearer dev-token-0123456789",
        "from-static",
        "//a:b",
    )
    .await;
    let client = server.client().await;
    let rows = client
        .query(
            "SELECT key, writer_ref, writer_run_id, writer_sha, writer_workflow \
             FROM cache.manifests ORDER BY key",
            &[],
        )
        .await
        .unwrap();
    let ci = &rows[0];
    assert_eq!(ci.get::<_, String>(0), "from-ci");
    assert_eq!(
        ci.get::<_, Option<String>>(1).as_deref(),
        Some("refs/heads/main")
    );
    assert_eq!(ci.get::<_, Option<String>>(2).as_deref(), Some("77"));
    assert_eq!(ci.get::<_, Option<String>>(3).as_deref(), Some("cafe"));
    assert!(ci.get::<_, Option<String>>(4).unwrap().contains("ci.yml"));
    let static_row = &rows[1];
    for column in 1..5 {
        assert_eq!(static_row.get::<_, Option<String>>(column), None);
    }
    drop(client);
    server.stop().await;
}

/// An engineer opens the cache page after CI has used the cache: two runs on
/// main write entries, one lookup hits a key and misses another, fifty older
/// entries fill the first page, and an IAP viewer reads the page in a browser.
#[tokio::test]
async fn the_cache_page_shows_what_ci_wrote_hit_rates_filter_and_paging() {
    let Some((server, _)) = server().await else {
        return;
    };

    // Phase 1: two CI runs on main commit one manifest each.
    put_blob(&server, DIGEST).await;
    let first = ci_writer("1001", "aaaa1111111111");
    let second = ci_writer("1002", "bbbb2222222222");
    commit_as(&server, &first, "key-one", "//apps/web:build").await;
    commit_as(&server, &second, "key-two", "//apps/api:test").await;

    // Phase 2: a lookup of one stored key and one unknown key counts one hit
    // and one miss.
    let (status, looked) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["key-one", "never-stored"]}),
            Some(&first),
            None,
        )
        .await;
    assert_eq!(status, 200, "{looked}");
    assert_eq!(wait_for_stats(&server.client().await, 1, 1).await, (1, 1));

    // Phase 3: fifty older manifests push the entries past one page of 50.
    let client = server.client().await;
    let base = Utc::now() - Duration::hours(1);
    for index in 0..50 {
        let manifest = format!("{{\"target\":\"//bulk:t{index:02}\"}}").into_bytes();
        client
            .execute(
                "INSERT INTO cache.manifests (repo, key, manifest, size_bytes, created_at, target) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &REPO,
                    &format!("bulk-{index:02}"),
                    &manifest,
                    &(manifest.len() as i64),
                    &(base - Duration::minutes(index)),
                    &format!("//bulk:t{index:02}"),
                ],
            )
            .await
            .unwrap();
    }
    drop(client);

    // Phase 4: a viewer inside the allowed domain opens the page.
    let viewer = common::iap_assertion(VIEWER);
    let path = format!("/ui/cache?repo={}", REPO.replace('/', "%2F"));
    let (status, _, html) = server.get_page(&path, Some(&viewer)).await;
    assert_eq!(status, 200, "{html}");
    // Counts: 52 manifests, the one shared blob, and the 1 hit of 2 lookups.
    assert!(html.contains("<dt>Manifests</dt><dd>52 ("), "{html}");
    assert!(html.contains("<dt>Blobs, all repositories</dt><dd>1 (4 B)</dd>"));
    assert!(html.contains("<td class=\"num\">50.0%</td>"), "{html}");
    assert!(html.contains("<th>Total</th>"));
    // The newest writer's run links to its GitHub Actions run.
    let run = format!("https://github.com/{REPO}/actions/runs/1002");
    assert!(
        html.contains(&format!("<a href=\"{run}\">1002</a>")),
        "{html}"
    );
    assert!(html.contains("<code>bbbb222222</code>"));
    // An entry row: target, its hit, and the run that wrote it.
    assert!(html.contains("<code>//apps/web:build</code>"));
    assert!(html.contains(&format!("{REPO}/actions/runs/1001")));
    assert!(html.contains("//apps/api:test"));

    // The first page holds 50 of 52 entries and links to the next page.
    assert!(html.contains("//bulk:t47") && !html.contains("//bulk:t48"));
    let next = html
        .split("<a href=\"")
        .map(|part| part.split('"').next().unwrap_or(""))
        .find(|href| href.contains("&amp;before="))
        .expect("a next page link")
        .replace("&amp;", "&");
    let (status, _, second_page) = server.get_page(&next, Some(&viewer)).await;
    assert_eq!(status, 200, "{second_page}");
    assert!(second_page.contains("//bulk:t48") && second_page.contains("//bulk:t49"));
    assert!(!second_page.contains("//apps/web:build") && !second_page.contains("Next page"));

    // The target filter narrows the entries and drops the next page link.
    let (status, _, filtered) = server
        .get_page(&format!("{path}&target=API"), Some(&viewer))
        .await;
    assert_eq!(status, 200, "{filtered}");
    assert!(filtered.contains("//apps/api:test"));
    assert!(!filtered.contains("//apps/web:build") && !filtered.contains("//bulk:"));
    assert!(!filtered.contains("Next page"));

    // Phase 5: who may not read it.
    let (status, _, _) = server
        .get_page(&path, Some(&common::iap_assertion("eve@other.test")))
        .await;
    assert_eq!(status, 403);
    let (status, _, _) = server.get_page(&path, None).await;
    assert_eq!(status, 401);
    let (status, _, _) = server
        .get_page("/ui/cache?repo=evil%2Fother", Some(&viewer))
        .await;
    assert_eq!(status, 403);
    let (status, _, _) = server
        .get_page(&format!("{path}&before=garbage"), Some(&viewer))
        .await;
    assert_eq!(status, 400);
    server.stop().await;
}

/// The stats row is a single hot row per repository. While another
/// transaction holds it, a lookup still answers at once with its hits, and the
/// blocked write does not hold the lookup back or fail it.
#[tokio::test]
async fn a_lookup_answers_quickly_while_the_stats_row_is_locked() {
    let Some((server, _)) = server().await else {
        return;
    };
    put_blob(&server, DIGEST).await;
    let writer = bearer(REPO, Some("refs/heads/main"));
    commit_as(&server, &writer, "k1", "//a:b").await;
    let mut locker = server.client().await;
    // Create today's row, then keep it locked in an open transaction.
    locker
        .execute(
            "INSERT INTO cache.lookup_stats (repo, day, hits, misses) \
             VALUES ($1, (now() AT TIME ZONE 'UTC')::date, 0, 0)",
            &[&REPO],
        )
        .await
        .unwrap();
    let hold = locker.transaction().await.unwrap();
    hold.execute(
        "UPDATE cache.lookup_stats SET hits = hits WHERE repo = $1",
        &[&REPO],
    )
    .await
    .unwrap();

    let started = std::time::Instant::now();
    for _ in 0..3 {
        let (status, body) = server
            .post_json(
                "/v1/cache/lookup",
                &json!({"repo": REPO, "keys": ["k1", "missing"]}),
                Some(&writer),
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["hits"].as_object().unwrap().len(), 1);
    }
    let took = started.elapsed();
    // The stats write waits up to its own timeout (3s); the lookups must not.
    assert!(took < std::time::Duration::from_secs(2), "took {took:?}");

    // Once the lock is released the queued writes land.
    hold.rollback().await.unwrap();
    drop(locker);
    let client = server.client().await;
    assert_eq!(wait_for_stats(&client, 3, 3).await, (3, 3));
    server.stop().await;
}

/// A lookup bumps `last_used_at`, `hits` and `last_hit_at` in one UPDATE, so
/// a repeated key in the request still counts one hit.
#[tokio::test]
async fn a_lookup_counts_one_hit_per_distinct_key() {
    let Some((server, _)) = server().await else {
        return;
    };
    put_blob(&server, DIGEST).await;
    let writer = bearer(REPO, Some("refs/heads/main"));
    commit_as(&server, &writer, "k1", "//a:b").await;
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k1", "k1", "k1"]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let client = server.client().await;
    let row = client
        .query_one(
            "SELECT hits, last_hit_at IS NOT NULL, last_used_at >= created_at \
             FROM cache.manifests WHERE key = 'k1'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert!(row.get::<_, bool>(1) && row.get::<_, bool>(2));
    assert_eq!(wait_for_stats(&client, 1, 0).await, (1, 0));
    server.stop().await;
}

async fn insert_entry(
    client: &deadpool_postgres::Client,
    key: &str,
    target: &str,
    created_at: chrono::DateTime<Utc>,
) {
    let manifest = serde_json::to_vec(&json!({"target": target})).unwrap();
    client
        .execute(
            "INSERT INTO cache.manifests (repo, key, manifest, size_bytes, created_at, target) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &REPO,
                &key,
                &manifest,
                &(manifest.len() as i64),
                &created_at,
                &target,
            ],
        )
        .await
        .unwrap();
}

async fn entry_targets(server: &common::Server, query: &str) -> (Vec<String>, Option<String>) {
    let viewer = common::person_bearer(server, VIEWER);
    let (status, page) = server
        .get_json(
            &format!("/v1/cache/entries?repo={REPO}&{query}"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{page}");
    let targets = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap().to_string())
        .collect();
    (targets, page["next"].as_str().map(str::to_string))
}

/// The target filter is a literal, case-insensitive substring: `%`, `_` and
/// `\` match themselves, and a page of matches is full even when most rows
/// in between do not match.
#[tokio::test]
async fn the_target_filter_matches_literally_and_fills_pages() {
    let Some((server, _)) = server().await else {
        return;
    };
    let client = server.client().await;
    let base = Utc::now();
    for (index, target) in [
        "//a:100%",
        "//a:1000",
        "//b_c",
        "//bxc",
        "//d\\e",
        "//dxe",
        "//Mixed:Case",
    ]
    .iter()
    .enumerate()
    {
        insert_entry(
            &client,
            &format!("lit-{index}"),
            target,
            base - Duration::minutes(index as i64),
        )
        .await;
    }
    assert_eq!(entry_targets(&server, "target=%25").await.0, ["//a:100%"]);
    assert_eq!(entry_targets(&server, "target=_").await.0, ["//b_c"]);
    assert_eq!(entry_targets(&server, "target=%5C").await.0, ["//d\\e"]);
    assert_eq!(entry_targets(&server, "target=b_c").await.0, ["//b_c"]);
    assert_eq!(
        entry_targets(&server, "target=mixed%3Acase").await.0,
        ["//Mixed:Case"]
    );

    // Ten matches among thirty rows that do not match, interleaved.
    for index in 0..40 {
        let target = if index % 4 == 0 {
            format!("//want:{index:02}")
        } else {
            format!("//other:{index:02}")
        };
        insert_entry(
            &client,
            &format!("fill-{index:02}"),
            &target,
            base + Duration::minutes(10 + index),
        )
        .await;
    }
    let mut sizes = Vec::new();
    let mut before = String::new();
    loop {
        let (targets, next) =
            entry_targets(&server, &format!("target=want&limit=3&before={before}")).await;
        sizes.push(targets.len());
        match next {
            Some(next) => before = next,
            None => break,
        }
    }
    assert_eq!(sizes, [3, 3, 3, 1]);
    server.stop().await;
}

/// Rows written before the denormalized columns existed are filled from their
/// manifest JSON by the v3 migration, and a manifest that does not decode keeps
/// the defaults.
#[tokio::test]
async fn migration_v3_backfills_rows_written_before_it() {
    let Some((server, _)) = server().await else {
        return;
    };
    let client = server.client().await;
    // Put the table back as v2 left it.
    client
        .batch_execute(
            "DROP INDEX cache.manifests_browse; \
             ALTER TABLE cache.manifests DROP COLUMN target, DROP COLUMN output_files, \
                 DROP COLUMN exit_code, DROP COLUMN duration_ms, DROP COLUMN aster_version",
        )
        .await
        .unwrap();
    let full = serde_json::to_vec(&json!({
        "target": "//apps/web:build", "exit_code": -1, "duration_ms": 42,
        "aster_version": "0.22.0", "outputs": [{"path": "x"}, {"path": "y"}],
    }))
    .unwrap();
    let rows: Vec<(&str, Vec<u8>)> =
        vec![
        ("full", full),
        ("not-utf8", b"\xff\xfe not json".to_vec()),
        ("nul-escape", br#"{"target":"a\u0000b","duration_ms":7}"#.to_vec()),
        ("array", b"[1,2]".to_vec()),
        (
            "wrong-types",
            br#"{"target":5,"outputs":"x","exit_code":"1","duration_ms":-3,"aster_version":[]}"#
                .to_vec(),
        ),
        (
            "non-numeric-strings",
            br#"{"target":"//s:t","exit_code":"abc","duration_ms":"1e999x"}"#.to_vec(),
        ),
        ("huge", br#"{"target":"//h:t","exit_code":99999999999,"duration_ms":1.5}"#.to_vec()),
    ];
    for (key, manifest) in &rows {
        client
            .execute(
                "INSERT INTO cache.manifests (repo, key, manifest, size_bytes) \
                 VALUES ($1, $2, $3, $4)",
                &[&REPO, key, manifest, &(manifest.len() as i64)],
            )
            .await
            .unwrap();
    }
    client
        .batch_execute(aster_srv::cache::MIGRATIONS[2].sql)
        .await
        .unwrap();
    let read = |key: &'static str| {
        let client = &client;
        async move {
            let row = client
                .query_one(
                    "SELECT target, output_files, exit_code, duration_ms, aster_version \
                     FROM cache.manifests WHERE key = $1",
                    &[&key],
                )
                .await
                .unwrap();
            (
                row.get::<_, String>(0),
                row.get::<_, i32>(1),
                row.get::<_, i32>(2),
                row.get::<_, i64>(3),
                row.get::<_, String>(4),
            )
        }
    };
    assert_eq!(
        read("full").await,
        ("//apps/web:build".into(), 2, -1, 42, "0.22.0".into())
    );
    let defaults = (String::new(), 0, 0, 0, String::new());
    assert_eq!(read("not-utf8").await, defaults);
    assert_eq!(read("nul-escape").await, defaults);
    assert_eq!(read("array").await, defaults);
    assert_eq!(read("wrong-types").await, defaults);
    assert_eq!(
        read("non-numeric-strings").await,
        ("//s:t".into(), 0, 0, 0, String::new())
    );
    assert_eq!(read("huge").await, ("//h:t".into(), 0, 0, 0, String::new()));
    // The browse page reads the backfilled rows.
    assert_eq!(
        entry_targets(&server, "target=apps").await.0,
        ["//apps/web:build"]
    );
    server.stop().await;
}

/// Commit stores the columns the browse page reads, from the decoded manifest.
#[tokio::test]
async fn commit_fills_the_browse_columns() {
    let Some((server, _)) = server().await else {
        return;
    };
    put_blob(&server, DIGEST).await;
    commit_as(
        &server,
        &bearer(REPO, Some("refs/heads/main")),
        "k1",
        "//apps/web:build",
    )
    .await;
    let row = server
        .client()
        .await
        .query_one(
            "SELECT target, output_files, exit_code, duration_ms, aster_version \
             FROM cache.manifests WHERE key = 'k1'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "//apps/web:build");
    assert_eq!(row.get::<_, i32>(1), 1);
    assert_eq!(row.get::<_, i32>(2), 0);
    assert_eq!(row.get::<_, i64>(3), 1500);
    assert_eq!(row.get::<_, String>(4), "0.22.0");
    server.stop().await;
}

/// Retention keeps lookup counts for the longer of 30 days and the cache's
/// own retention, and drops older days.
#[tokio::test]
async fn retention_prunes_old_lookup_stats() {
    let Some((server, _)) = server().await else {
        return;
    };
    let client = server.client().await;
    client
        .batch_execute(&format!(
            "INSERT INTO cache.lookup_stats (repo, day, hits, misses) VALUES \
             ('{REPO}', (now() AT TIME ZONE 'UTC')::date - 45, 1, 1), \
             ('{REPO}', (now() AT TIME ZONE 'UTC')::date - 31, 1, 1), \
             ('{REPO}', (now() AT TIME ZONE 'UTC')::date - 20, 1, 1), \
             ('{REPO}', (now() AT TIME ZONE 'UTC')::date, 1, 1)"
        ))
        .await
        .unwrap();
    aster_srv::cache::run_retention(&server.app, Utc::now())
        .await
        .unwrap()
        .expect("this replica holds the lock");
    let left: Vec<i32> = client
        .query(
            "SELECT ((now() AT TIME ZONE 'UTC')::date - day)::int FROM cache.lookup_stats \
             ORDER BY day",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(left, [20, 0]);
    server.stop().await;
}

/// N13. A pull-request CI token may read the cache, but its lookups must not
/// bump `hits`, `last_hit_at` or `last_used_at`: any PR author could otherwise
/// keep an entry alive forever. A person's lookup does not count either. A
/// main push (writer-eligible) still counts.
#[tokio::test]
async fn a_pull_request_lookup_does_not_count_but_a_main_push_does() {
    let Some((server, _)) = server().await else {
        return;
    };
    // Phase 1: main commits one entry.
    put_blob(&server, DIGEST).await;
    let writer = bearer(REPO, Some("refs/heads/main"));
    commit_as(&server, &writer, "k1", "//a:b").await;
    let used_before: chrono::DateTime<Utc> = server
        .client()
        .await
        .query_one(
            "SELECT last_used_at FROM cache.manifests WHERE key = 'k1'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    // Phase 2: a pull_request token looks the key up. It hits, but nothing is
    // counted in the manifest row.
    let pull_request = format!(
        "Bearer {}",
        common::token_with_claims(
            REPO,
            json!({"event_name": "pull_request", "ref": "refs/pull/7/merge"}),
        )
    );
    let (status, body) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k1"]}),
            Some(&pull_request),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert!(body["hits"]["k1"].is_object(), "{body}");
    let state = |client: deadpool_postgres::Client| async move {
        let row = client
            .query_one(
                "SELECT hits, last_hit_at IS NOT NULL, last_used_at FROM cache.manifests \
                 WHERE key = 'k1'",
                &[],
            )
            .await
            .unwrap();
        (
            row.get::<_, i64>(0),
            row.get::<_, bool>(1),
            row.get::<_, chrono::DateTime<Utc>>(2),
        )
    };
    assert_eq!(state(server.client().await).await, (0, false, used_before));

    // Phase 3: a signed-in person looks it up. People never count either.
    let (status, body) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k1"]}),
            Some(&common::person_bearer(&server, VIEWER)),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["hits"]["k1"].is_object(), "{body}");
    assert_eq!(state(server.client().await).await, (0, false, used_before));

    // Phase 4: a main push looks it up and is counted.
    let (status, _) = server
        .post_json(
            "/v1/cache/lookup",
            &json!({"repo": REPO, "keys": ["k1"]}),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let (hits, hit_at, used) = state(server.client().await).await;
    assert_eq!((hits, hit_at), (1, true));
    assert!(used >= used_before);
    server.stop().await;
}
