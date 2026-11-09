//! One scenario's server: the real routes, migrations and retention pass on a
//! fresh Postgres database, with the in-memory bucket. Nothing here decides
//! anything the model decides. It sends requests and reads state back.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use aster_srv::cache::{run_retention, BlobStore, FixedSigner};
use base64::Engine;
use chrono::Utc;
use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::common::{self, REPO};
use crate::generated_refinement::Implementation;

const NAMESPACE: &str = "dir/v1/0123456789abcdef";
/// The highest blob number a scenario uses, to turn digests back into blobs.
const BLOBS: i64 = 9;

fn digest_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A blob's content: always 10 bytes, the size the model's cap assumes.
fn content(blob: i64) -> Vec<u8> {
    format!("blob-{blob:05}").into_bytes()
}

fn digest(blob: i64) -> String {
    digest_of(&content(blob))
}

fn blob_named(name: &str) -> i64 {
    (1..=BLOBS)
        .find(|blob| digest(*blob) == name)
        .unwrap_or_else(|| panic!("{name} is not a scenario's blob"))
}

/// Key 11 is generation 1 of family 10.
fn key_name(key: i64) -> String {
    format!("{NAMESPACE}/{}-{}", key - key % 10, key % 10)
}

fn key_number(name: &str) -> i64 {
    let (family, generation) = name
        .rsplit('/')
        .next()
        .and_then(|last| last.split_once('-'))
        .unwrap_or_else(|| panic!("{name} is not a scenario's key"));
    family.parse::<i64>().unwrap() + generation.parse::<i64>().unwrap()
}

fn manifest_blobs(manifest: &[u8]) -> BTreeSet<i64> {
    let parsed: Value = serde_json::from_slice(manifest).expect("a stored manifest is JSON");
    parsed["outputs"]
        .as_array()
        .expect("a manifest has outputs")
        .iter()
        .map(|output| blob_named(output["digest"].as_str().expect("an output has a digest")))
        .collect()
}

fn int(value: &RuntimeValue) -> Result<i64, String> {
    match value {
        RuntimeValue::Int(number) => Ok(*number),
        other => Err(format!("expected an int, got {other:?}")),
    }
}

fn ints(value: &RuntimeValue) -> Result<Vec<i64>, String> {
    match value {
        RuntimeValue::Set(items) => items.iter().map(int).collect(),
        RuntimeValue::List(items) => items.iter().map(int).collect(),
        other => Err(format!("expected a collection, got {other:?}")),
    }
}

fn set(items: &BTreeSet<i64>) -> RuntimeValue {
    RuntimeValue::Set(items.iter().copied().map(RuntimeValue::Int).collect())
}

fn record(fields: Vec<(&str, RuntimeValue)>) -> RuntimeValue {
    RuntimeValue::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

/// Positions in a key list that is already in the server's own order.
fn positions(keys: Vec<i64>) -> RuntimeValue {
    RuntimeValue::Map(
        keys.into_iter()
            .enumerate()
            .map(|(at, key)| (RuntimeValue::Int(key), RuntimeValue::Int(at as i64)))
            .collect(),
    )
}

#[derive(Clone)]
pub struct Snapshot {
    state: RuntimeValue,
}

impl NormalizedRuntimeEvidence for Snapshot {
    fn resolve_name(&self, name: &str) -> Result<RuntimeValue, String> {
        match name {
            "state" => Ok(self.state.clone()),
            other => Err(format!("unknown evidence name {other}")),
        }
    }

    fn resolve_call(
        &self,
        _operator: &str,
        _arguments: &[RuntimeValue],
    ) -> Option<Result<RuntimeValue, String>> {
        None
    }
}

pub struct LiveCache {
    runtime: tokio::runtime::Runtime,
    server: Option<common::Server>,
    writer: String,
    reply: String,
    missing: BTreeSet<i64>,
    hit: (bool, i64, BTreeSet<i64>),
}

impl LiveCache {
    fn server(&self) -> &common::Server {
        self.server.as_ref().expect("the server runs until drop")
    }

    fn store(&self) -> &BlobStore {
        &self.server().app.cache_store
    }

    fn put(&self, blob: i64, bytes: Vec<u8>) -> Result<(), String> {
        self.runtime
            .block_on(self.store().put_unrecorded(&digest(blob), bytes.into()))
            .map_err(|error| error.to_string())
    }

    fn one_int(name: &str, arguments: &[RuntimeValue]) -> Result<i64, String> {
        match arguments {
            [value] => int(value),
            other => Err(format!("{name} expects one int: {other:?}")),
        }
    }

    async fn read_state(&self) -> RuntimeValue {
        let store = self.store();
        let client = self.server().app.pool.get().await.expect("a connection");

        let mut objects = BTreeSet::new();
        let mut corrupt = BTreeSet::new();
        for name in store.objects().await {
            let blob = blob_named(&name);
            objects.insert(blob);
            let bytes = store.get(&name).await.unwrap().expect("a listed object");
            if digest_of(&bytes) != name {
                corrupt.insert(blob);
            }
        }

        let rows: BTreeSet<i64> = client
            .query("SELECT sha256 FROM cache.blobs", &[])
            .await
            .expect("blob rows")
            .iter()
            .map(|row| blob_named(row.get(0)))
            .collect();

        let mut entries = BTreeMap::new();
        for row in client
            .query(
                "SELECT key, manifest FROM cache.manifests WHERE key LIKE 'dir/v1/%'",
                &[],
            )
            .await
            .expect("manifests")
        {
            let manifest: Vec<u8> = row.get(1);
            entries.insert(
                RuntimeValue::Int(key_number(row.get(0))),
                set(&manifest_blobs(&manifest)),
            );
        }

        // The two orders retention and restore read, with the same ORDER BY.
        let order = |query: &'static str| async {
            let rows = client.query(query, &[]).await.expect("an ordering");
            let keys = rows.iter().map(|row| key_number(row.get(0))).collect();
            positions(keys)
        };
        let recent = order(
            "SELECT key FROM cache.manifests WHERE key LIKE 'dir/v1/%' \
             ORDER BY last_used_at DESC, created_at DESC",
        )
        .await;
        let newest = order(
            "SELECT key FROM cache.manifests WHERE key LIKE 'dir/v1/%' ORDER BY created_at DESC",
        )
        .await;

        let (found, key, blobs) = &self.hit;
        record(vec![
            ("objects", set(&objects)),
            ("corrupt", set(&corrupt)),
            ("rows", set(&rows)),
            ("entries", RuntimeValue::Map(entries)),
            ("recent", recent),
            ("newest", newest),
            ("reply", RuntimeValue::Text(self.reply.clone())),
            ("missing", set(&self.missing)),
            (
                "hit",
                record(vec![
                    ("found", RuntimeValue::Bool(*found)),
                    ("key", RuntimeValue::Int(*key)),
                    ("blobs", set(blobs)),
                ]),
            ),
        ])
    }
}

impl Drop for LiveCache {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            self.runtime.block_on(server.stop());
        }
    }
}

impl Implementation for LiveCache {
    type Evidence = Snapshot;

    fn from_initial_state(_initial: &RuntimeValue) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let signer = Arc::new(FixedSigner {
            email: "cache-test@example.iam.gserviceaccount.com".into(),
            signature: b"test-signature".to_vec(),
            signed: Default::default(),
        });
        let server = runtime
            .block_on(common::start_with(
                &[
                    ("ASTER_SRV_CACHE_BUCKET", "aster-cache-test"),
                    (
                        "ASTER_SRV_CACHE_SIGNER",
                        "cache-test@example.iam.gserviceaccount.com",
                    ),
                    ("ASTER_SRV_CACHE_DAYS", "14"),
                    ("ASTER_SRV_DIR_CACHE_MAX_BYTES", "25"),
                ],
                Some(BlobStore::memory("aster-cache-test", signer)),
            ))
            .ok_or(
                "set ASTER_SRV_TEST_DATABASE_URL: this check has nothing to run without Postgres",
            )?;
        let writer = format!(
            "Bearer {}",
            common::token_with_ref(REPO, Some("refs/heads/main"))
        );
        Ok(LiveCache {
            runtime,
            server: Some(server),
            writer,
            reply: "none".into(),
            missing: BTreeSet::new(),
            hit: (false, 0, BTreeSet::new()),
        })
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: self.runtime.block_on(self.read_state()),
        }
    }

    // A signed PUT: bytes reach the bucket and nothing is recorded.
    fn put_object(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let blob = Self::one_int("putObject", arguments)?;
        self.put(blob, content(blob))
    }

    fn put_forged(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let blob = Self::one_int("putForged", arguments)?;
        self.put(blob, b"forged".to_vec())
    }

    // The bucket lost an object: a lifecycle rule or an operator.
    fn lose(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let blob = Self::one_int("lose", arguments)?;
        self.runtime
            .block_on(self.store().delete(&digest(blob)))
            .map_err(|error| error.to_string())
    }

    fn request_upload(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [blobs] = arguments else {
            return Err(format!("requestUpload expects blobs: {arguments:?}"));
        };
        let digests: Vec<String> = ints(blobs)?.into_iter().map(digest).collect();
        let body = json!({"repo": REPO, "digests": digests});
        let (status, reply) = self.runtime.block_on(self.server().post_json(
            "/v1/cache/upload",
            &body,
            Some(&self.writer),
            None,
        ));
        if status == 200 {
            Ok(())
        } else {
            Err(format!("upload answered {status}: {reply}"))
        }
    }

    fn commit_entry(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [key, blobs] = arguments else {
            return Err(format!(
                "commitEntry expects a key and blobs: {arguments:?}"
            ));
        };
        let key = key_name(int(key)?);
        let outputs: Vec<Value> = ints(blobs)?
            .into_iter()
            .enumerate()
            .map(|(index, blob)| {
                json!({
                    "path": format!("chunk/{index:06}"),
                    "kind": "file",
                    "digest": digest(blob),
                    "mode": 420,
                })
            })
            .collect();
        let manifest = serde_json::to_vec(&json!({
            "schema": 1,
            "key": key,
            "target": "//:dir-cache",
            "outputs": outputs,
        }))
        .map_err(|error| error.to_string())?;
        let body = json!({
            "repo": REPO,
            "key": key,
            "manifest": base64::engine::general_purpose::STANDARD.encode(manifest),
        });
        let (status, reply) = self.runtime.block_on(self.server().post_json(
            "/v1/cache/commit",
            &body,
            Some(&self.writer),
            None,
        ));
        self.missing = BTreeSet::new();
        self.reply = match status {
            201 => "stored".into(),
            409 => "exists".into(),
            422 => {
                self.missing = reply["missing"]
                    .as_array()
                    .ok_or_else(|| format!("a 422 without missing blobs: {reply}"))?
                    .iter()
                    .map(|name| blob_named(name.as_str().expect("a digest")))
                    .collect();
                "missing".into()
            }
            other => return Err(format!("commit answered {other}: {reply}")),
        };
        Ok(())
    }

    fn dir_lookup(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [key, families] = arguments else {
            return Err(format!(
                "dirLookup expects a key and families: {arguments:?}"
            ));
        };
        let prefixes: Vec<String> = ints(families)?
            .into_iter()
            .map(|family| format!("{NAMESPACE}/{family}-"))
            .collect();
        let body = json!({"repo": REPO, "key": key_name(int(key)?), "prefixes": prefixes});
        let (status, reply) = self.runtime.block_on(self.server().post_json(
            "/v1/cache/dir-lookup",
            &body,
            Some(&self.writer),
            None,
        ));
        if status != 200 {
            return Err(format!("dir-lookup answered {status}: {reply}"));
        }
        self.hit = match reply["hit"].as_object() {
            None => (false, 0, BTreeSet::new()),
            Some(hit) => {
                let manifest = base64::engine::general_purpose::STANDARD
                    .decode(hit["manifest"].as_str().expect("a hit has a manifest"))
                    .map_err(|error| error.to_string())?;
                let key = key_number(hit["key"].as_str().expect("a hit has a key"));
                (true, key, manifest_blobs(&manifest))
            }
        };
        Ok(())
    }

    // Make the expired keys old enough, then run the real pass.
    fn run_retention(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [expired] = arguments else {
            return Err(format!("runRetention expects keys: {arguments:?}"));
        };
        let keys: Vec<String> = ints(expired)?.into_iter().map(key_name).collect();
        let server = self.server();
        self.runtime.block_on(async {
            let client = server.app.pool.get().await.map_err(|e| e.to_string())?;
            client
                .execute(
                    "UPDATE cache.manifests SET last_used_at = now() - interval '15 days' \
                     WHERE key = ANY($1)",
                    &[&keys],
                )
                .await
                .map_err(|e| e.to_string())?;
            drop(client);
            run_retention(&server.app, Utc::now())
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}
