//! Executor-facing artifact cache.
//!
//! Manifests and blobs live in [`LocalStore`]. A per-project index maps a
//! target address to the key that was stored, so lookup, status, and
//! invalidation do not scan the action cache. `.aster/cache/cache.json` is
//! obsolete and is neither read nor written.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::artifact::Manifest;
use super::backend::TieredCache;
use super::local::LocalStore;

/// One address in a project index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Hex key from [`super::CacheHasher::compute_hash`].
    pub key: String,
    /// RFC 3339 timestamp copied from the stored manifest.
    pub created_at: String,
    /// Set when [`ArtifactCache::remove`] dropped this address. The manifest
    /// stays, but lookup must not restore it until something stores again.
    #[serde(default)]
    removed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectIndex {
    entries: BTreeMap<String, IndexEntry>,
}

/// One indexed target, as `aster cache status` prints it.
///
/// `exit_code` is null when the index names a key whose manifest is missing.
/// `output_bytes` is the on-disk size of the manifest's output blobs, not the
/// log blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatusTarget {
    pub address: String,
    pub key: String,
    pub created_at: String,
    pub outputs: u64,
    pub output_bytes: u64,
    pub exit_code: Option<i32>,
}

/// Status document `aster cache status` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatus {
    pub cache_dir: String,
    pub targets: Vec<CacheStatusTarget>,
}

/// Artifact cache: project index plus a [`TieredCache`].
///
/// Lookup and store go through the tier. The index stays local. Remote
/// selection uses `ASTER_REMOTE_CACHE` and `ASTER_CACHE_MODE`.
pub struct ArtifactCache {
    cache_dir: PathBuf,
    tier: TieredCache,
}

impl ArtifactCache {
    /// `cache_dir` is the resolved cache directory (`cas/`, `ac/`, `index/`).
    /// `workspace_root` is the checkout, used to name the remote cache repo.
    ///
    /// The remote comes from the environment. A missing credential or a
    /// not-yet-implemented remote is local-only and does not fail construction.
    pub fn new(cache_dir: PathBuf, workspace_root: &Path) -> Self {
        let tier = super::backend::tier_from_env(&cache_dir, workspace_root).unwrap_or_else(|error| {
            eprintln!(
                "[aster] Warning: remote cache config ignored, using the local cache only: {error:#}"
            );
            TieredCache::local_only(LocalStore::new(cache_dir.clone()))
        });
        Self { cache_dir, tier }
    }

    /// Same as [`Self::new`], but lookup and store use `tier`.
    ///
    /// Tests pass a [`TieredCache`] wrapped around an in-memory remote.
    /// Production uses [`Self::new`], which selects the remote from the environment.
    pub fn with_tier(cache_dir: PathBuf, tier: TieredCache) -> Self {
        Self { cache_dir, tier }
    }

    /// Directory that holds `cas/`, `ac/`, `index/`, and `cache.lock`.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The local blob and manifest store. Restore and pack read this, not the remote.
    pub fn local_store(&self) -> &LocalStore {
        self.tier.local_store()
    }

    /// Batch-lookup `keys` on the remote before this level runs.
    pub fn prefetch(&self, keys: &[String]) {
        self.tier.prefetch(keys);
    }

    /// Manifest for `address` when the index key matches `key`.
    ///
    /// A missing index entry asks the tier, so a remote hit can restore
    /// into a checkout that has never stored this address. A different key
    /// is a miss. A missing manifest, or one that does not decode, is a miss.
    pub fn lookup(&self, project: &str, address: &str, key: &str) -> Result<Option<Manifest>> {
        let _lock = self.lock_shared()?;
        let index = self.read_index(project)?;
        if let Some(entry) = index.entries.get(address) {
            if entry.removed || entry.key != key {
                return Ok(None);
            }
        }
        // No index row is a local miss, not a remote miss: another checkout
        // may have stored this key. A different key is a real miss.
        let manifest = self.tier.lookup(key)?;
        let Some(manifest) = manifest else {
            return Ok(None);
        };
        // A remote hit for another address must not be claimed by this one.
        if manifest.target != address {
            return Ok(None);
        }
        if index.entries.get(address).is_none_or(|entry| entry.removed) {
            let created_at = manifest.created_at.clone();
            drop(_lock);
            let _exclusive = self.lock_exclusive()?;
            let mut index = self.read_index(project)?;
            index
                .entries
                .entry(address.to_string())
                .or_insert(IndexEntry {
                    key: key.to_string(),
                    created_at,
                    removed: false,
                });
            self.write_index(project, &index)?;
        }
        Ok(Some(manifest))
    }

    /// Store `manifest`, then record `address` in the project index.
    ///
    /// Blobs must already be in the local store: [`LocalStore::put_manifest`]
    /// refuses a manifest whose blobs are missing. A configured remote is
    /// uploaded after the local write and cannot fail this call.
    pub fn store(&self, project: &str, address: &str, manifest: &Manifest) -> Result<()> {
        self.tier.store(manifest)?;
        let _lock = self.lock_exclusive()?;
        let mut index = self.read_index(project)?;
        index.entries.insert(
            address.to_string(),
            IndexEntry {
                key: manifest.key.clone(),
                created_at: manifest.created_at.clone(),
                removed: false,
            },
        );
        self.write_index(project, &index)
    }

    /// Index rows for `project`, in address order.
    pub fn list_project_entries(&self, project: &str) -> Result<Vec<(String, IndexEntry)>> {
        let _lock = self.lock_shared()?;
        let index = self.read_index(project)?;
        Ok(index
            .entries
            .into_iter()
            .filter(|(_, entry)| !entry.removed)
            .collect())
    }

    /// Delete every index entry for `project`. Blobs and manifests stay.
    pub fn invalidate_project(&self, project: &str) -> Result<()> {
        let _lock = self.lock_exclusive()?;
        let path = self.index_path(project);
        if path.is_file() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove {}", path.display()))?;
        }
        Ok(())
    }

    /// Delete the index entry for `address`. Blobs and manifests stay.
    pub fn remove(&self, address: &str) -> Result<()> {
        let Some(project) = project_of(address) else {
            return Ok(());
        };
        let _lock = self.lock_exclusive()?;
        let mut index = self.read_index(project)?;
        if let Some(entry) = index.entries.get_mut(address) {
            entry.removed = true;
        } else {
            return Ok(());
        }
        self.write_index(project, &index)
    }

    /// Every indexed target, joined to its manifest, in address order.
    ///
    /// A missing manifest still appears, with `outputs` 0, `output_bytes` 0,
    /// and `exit_code` null. `filter` keeps one address, or every address in
    /// a project (`//project`).
    pub fn status(&self, filter: Option<&str>) -> Result<CacheStatus> {
        let _lock = self.lock_shared()?;
        let mut targets = Vec::new();
        let dir = self.cache_dir.join("index");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CacheStatus {
                    cache_dir: self.cache_dir.display().to_string(),
                    targets,
                })
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", dir.display()))
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("Failed to read {}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let index = read_index_file(&path)?;
            for (address, row) in index.entries {
                if row.removed || !status_matches(filter, &address) {
                    continue;
                }
                let manifest = self.tier.local_store().get_manifest(&row.key)?;
                let (outputs, output_bytes, exit_code) = match &manifest {
                    Some(manifest) => (
                        manifest.outputs.len() as u64,
                        self.output_bytes(manifest)?,
                        Some(manifest.exit_code),
                    ),
                    None => (0, 0, None),
                };
                targets.push(CacheStatusTarget {
                    address,
                    key: row.key,
                    created_at: row.created_at,
                    outputs,
                    output_bytes,
                    exit_code,
                });
            }
        }
        targets.sort_by(|left, right| left.address.cmp(&right.address));
        Ok(CacheStatus {
            cache_dir: self.cache_dir.display().to_string(),
            targets,
        })
    }

    fn output_bytes(&self, manifest: &Manifest) -> Result<u64> {
        let mut total = 0u64;
        for output in &manifest.outputs {
            let Some(digest) = &output.digest else {
                continue;
            };
            let path = self.tier.local_store().blob_path(digest)?;
            match fs::metadata(&path) {
                Ok(meta) => total += meta.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("Failed to stat {}", path.display()))
                }
            }
        }
        Ok(total)
    }

    /// Delete every project index. Manifests and blobs stay for prune.
    pub fn clear(&self) -> Result<()> {
        let _lock = self.lock_exclusive()?;
        let dir = self.cache_dir.join("index");
        if dir.is_dir() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to remove {}", dir.display()))?;
        }
        Ok(())
    }

    fn index_path(&self, project: &str) -> PathBuf {
        self.cache_dir
            .join("index")
            .join(format!("{}.json", url_safe(project)))
    }

    fn read_index(&self, project: &str) -> Result<ProjectIndex> {
        read_index_file(&self.index_path(project))
    }

    fn write_index(&self, project: &str, index: &ProjectIndex) -> Result<()> {
        let path = self.index_path(project);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        write_index_file(&path, index)
    }

    fn lock_shared(&self) -> Result<File> {
        self.lock(true)
    }

    fn lock_exclusive(&self) -> Result<File> {
        self.lock(false)
    }

    fn lock(&self, shared: bool) -> Result<File> {
        fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("Failed to create {}", self.cache_dir.display()))?;
        let path = self.cache_dir.join("cache.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Failed to open lock file {}", path.display()))?;
        if shared {
            FileExt::lock_shared(&file).context("Failed to acquire shared cache lock")?;
        } else {
            FileExt::lock_exclusive(&file).context("Failed to acquire cache lock")?;
        }
        Ok(file)
    }
}

fn read_index_file(path: &Path) -> Result<ProjectIndex> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectIndex::default())
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()))
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(index) => Ok(index),
        Err(error) => {
            eprintln!(
                "[aster] Warning: failed to parse {}, treating project index as empty: {error}",
                path.display()
            );
            Ok(ProjectIndex::default())
        }
    }
}

fn write_index_file(path: &Path, index: &ProjectIndex) -> Result<()> {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("index"),
        std::process::id(),
        seq
    ));
    let bytes = serde_json::to_vec_pretty(index).context("Failed to encode project index")?;
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("Failed to open {}", tmp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// `//project:target` → `//project`.
pub fn project_of(address: &str) -> Option<&str> {
    address.rsplit_once(':').map(|(project, _)| project)
}

/// `None` matches every address. An address matches itself. A project
/// matches addresses whose project component is exactly that project, so
/// `//app` does not match `//app-v2` or `//apps/web`.
fn status_matches(filter: Option<&str>, address: &str) -> bool {
    match filter {
        None => true,
        Some(filter) if filter.contains(':') => filter == address,
        Some(filter) => project_of(address) == Some(filter),
    }
}

/// Percent-encode everything but unreserved characters, so a project address
/// is one filename on every platform.
fn url_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::artifact::{Digest, OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION};
    use crate::cache::CacheBackend;

    fn manifest(key: &str, target: &str) -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: key.to_string(),
            target: target.to_string(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-17T00:00:00Z".to_string(),
            outputs: Vec::new(),
            log: None,
            aster_version: "0.21.0".to_string(),
            packs: Vec::new(),
        }
    }

    #[test]
    fn lookup_requires_matching_index_key_and_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(tmp.path().to_path_buf(), tmp.path());
        let key = "ab".repeat(32);
        let stored = manifest(&key, "//app:build");
        cache.store("//app", "//app:build", &stored).unwrap();

        let hit = cache
            .lookup("//app", "//app:build", &key)
            .unwrap()
            .expect("hit");
        assert_eq!(hit.key, key);
        assert!(cache
            .lookup("//app", "//app:build", &"cd".repeat(32))
            .unwrap()
            .is_none());
        assert!(cache
            .lookup("//app", "//app:other", &key)
            .unwrap()
            .is_none());
    }

    #[test]
    fn remove_and_invalidate_drop_index_entries_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(tmp.path().to_path_buf(), tmp.path());
        let key = "11".repeat(32);
        cache
            .store("//app", "//app:build", &manifest(&key, "//app:build"))
            .unwrap();
        cache.remove("//app:build").unwrap();
        assert!(cache
            .lookup("//app", "//app:build", &key)
            .unwrap()
            .is_none());
        assert!(cache.local_store().get_manifest(&key).unwrap().is_some());

        cache
            .store("//app", "//app:build", &manifest(&key, "//app:build"))
            .unwrap();
        cache.invalidate_project("//app").unwrap();
        assert!(cache.list_project_entries("//app").unwrap().is_empty());
    }

    #[test]
    fn status_lists_indexed_targets_and_clear_matching_is_project_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(tmp.path().to_path_buf(), tmp.path());
        let blob = Digest::of_bytes(b"log");
        cache.local_store().put_blob(&blob, &b"log"[..]).unwrap();
        let mut with_log = manifest(&"aa".repeat(32), "//apps/web:build");
        with_log.log = Some(blob);
        with_log.outputs.push(OutputEntry {
            path: "out/file.txt".into(),
            kind: OutputKind::Dir,
            digest: None,
            mode: 0o755,
            target: None,
        });
        cache
            .store("//apps/web", "//apps/web:build", &with_log)
            .unwrap();
        cache
            .store(
                "//apps/api",
                "//apps/api:build",
                &manifest(&"bb".repeat(32), "//apps/api:build"),
            )
            .unwrap();

        let status = cache.status(None).unwrap();
        let web = status
            .targets
            .iter()
            .find(|row| row.address == "//apps/web:build")
            .expect("web entry");
        assert_eq!(web.key, "aa".repeat(32));
        assert_eq!(web.outputs, 1);
        assert_eq!(web.exit_code, Some(0));
        assert_eq!(web.output_bytes, 0, "a directory output has no blob");

        cache.invalidate_project("//apps/web").unwrap();
        let remaining = cache.status(None).unwrap();
        assert!(remaining
            .targets
            .iter()
            .any(|row| row.address == "//apps/api:build"));
        assert!(!remaining
            .targets
            .iter()
            .any(|row| row.address == "//apps/web:build"));
    }

    #[test]
    fn status_joins_manifest_outputs_and_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(tmp.path().to_path_buf(), tmp.path());
        let bytes = b"hello artifact";
        let digest = Digest::of_bytes(bytes);
        cache.local_store().put_blob(&digest, &bytes[..]).unwrap();
        let mut stored = manifest(&"cc".repeat(32), "//p:build");
        stored.outputs.push(OutputEntry {
            path: "out/file.txt".into(),
            kind: OutputKind::File,
            digest: Some(digest),
            mode: 0o644,
            target: None,
        });
        cache.store("//p", "//p:build", &stored).unwrap();
        cache
            .store(
                "//p-v2",
                "//p-v2:build",
                &manifest(&"dd".repeat(32), "//p-v2:build"),
            )
            .unwrap();

        let status = cache.status(Some("//p")).unwrap();
        assert_eq!(status.cache_dir, tmp.path().display().to_string());
        assert_eq!(status.targets.len(), 1);
        let row = &status.targets[0];
        assert_eq!(row.address, "//p:build");
        assert_eq!(row.key, "cc".repeat(32));
        assert_eq!(row.outputs, 1);
        assert_eq!(row.exit_code, Some(0));
        assert!(row.output_bytes > 0);

        let filtered = cache.status(Some("//p:build")).unwrap();
        assert_eq!(filtered.targets.len(), 1);
        assert!(cache
            .status(Some("//other:build"))
            .unwrap()
            .targets
            .is_empty());
        assert!(
            cache
                .status(Some("//p"))
                .unwrap()
                .targets
                .iter()
                .all(|row| row.address == "//p:build"),
            "//p must not match //p-v2"
        );
    }

    #[test]
    fn tiered_lookup_downloads_remote_hit_into_local_store() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();
        let digest = Digest::of_bytes(b"from-remote");
        let key = "ab".repeat(32);
        let stored = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: key.clone(),
            target: "//app:build".to_string(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-17T00:00:00Z".to_string(),
            outputs: vec![OutputEntry {
                path: "out/file.txt".into(),
                kind: OutputKind::File,
                digest: Some(digest.clone()),
                mode: 0o644,
                target: None,
            }],
            log: None,
            aster_version: "0.21.0".to_string(),
            packs: Vec::new(),
        };
        let remote = crate::cache::InMemoryBackend::new();
        let tier = crate::cache::TieredCache::with_remote(
            crate::cache::LocalStore::new(cache_dir.clone()),
            crate::cache::CacheMode::ReadWrite,
            std::sync::Arc::new(remote.clone()),
        );
        let cache = ArtifactCache::with_tier(cache_dir, tier);
        // Index the key with an empty local manifest, then delete that file
        // so lookup has nothing local. Seed the remote only after that store,
        // or the upload would overwrite the remote artifact with the empty one.
        cache
            .store("//app", "//app:build", &manifest(&key, "//app:build"))
            .unwrap();
        let manifest_path = cache
            .cache_dir()
            .join("ac")
            .join(&key[..2])
            .join(format!("{key}.json"));
        std::fs::remove_file(&manifest_path).unwrap();
        assert!(cache.local_store().get_manifest(&key).unwrap().is_none());
        remote
            .put_blob(&digest, &mut b"from-remote".to_vec().as_slice())
            .unwrap();
        remote.put_manifest(&stored).unwrap();

        let hit = cache
            .lookup("//app", "//app:build", &key)
            .unwrap()
            .expect("hit");
        assert_eq!(hit, stored);
        let mut restored = Vec::new();
        cache
            .local_store()
            .get_blob(&digest, &mut restored)
            .unwrap();
        assert_eq!(restored, b"from-remote");
    }
}
