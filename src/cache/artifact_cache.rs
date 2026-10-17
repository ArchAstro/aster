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
use super::local::LocalStore;

/// One address in a project index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Hex key from [`super::CacheHasher::compute_hash`].
    pub key: String,
    /// RFC 3339 timestamp copied from the stored manifest.
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectIndex {
    entries: BTreeMap<String, IndexEntry>,
}

/// Status row shaped like the obsolete `cache.json` entry.
///
/// `success` is always true: only exit code 0 is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub hash: String,
    pub timestamp: String,
    pub success: bool,
}

/// Status document shaped like the obsolete `CacheState`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusState {
    pub targets: BTreeMap<String, StatusEntry>,
}

/// Local artifact cache: index plus [`LocalStore`].
pub struct ArtifactCache {
    cache_dir: PathBuf,
    store: LocalStore,
}

impl ArtifactCache {
    /// `cache_dir` is the resolved cache directory (`cas/`, `ac/`, `index/`).
    pub fn new(cache_dir: PathBuf) -> Self {
        let store = LocalStore::new(cache_dir.clone());
        Self { cache_dir, store }
    }

    /// Directory that holds `cas/`, `ac/`, `index/`, and `cache.lock`.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The local blob and manifest store.
    pub fn local_store(&self) -> &LocalStore {
        &self.store
    }

    /// Manifest for `address` when the index key matches `key`.
    ///
    /// A missing index entry, a different key, a missing manifest, or a
    /// manifest that does not decode is a miss (`Ok(None)`).
    pub fn lookup(&self, project: &str, address: &str, key: &str) -> Result<Option<Manifest>> {
        let _lock = self.lock_shared()?;
        let index = self.read_index(project)?;
        let Some(entry) = index.entries.get(address) else {
            return Ok(None);
        };
        if entry.key != key {
            return Ok(None);
        }
        self.store.get_manifest(key)
    }

    /// Store `manifest`, then record `address` in the project index.
    ///
    /// Blobs must already be in the store: [`LocalStore::put_manifest`]
    /// refuses a manifest whose blobs are missing.
    pub fn store(&self, project: &str, address: &str, manifest: &Manifest) -> Result<()> {
        self.store.put_manifest(manifest)?;
        let _lock = self.lock_exclusive()?;
        let mut index = self.read_index(project)?;
        index.entries.insert(
            address.to_string(),
            IndexEntry {
                key: manifest.key.clone(),
                created_at: manifest.created_at.clone(),
            },
        );
        self.write_index(project, &index)
    }

    /// Index rows for `project`, in address order.
    pub fn list_project_entries(&self, project: &str) -> Result<Vec<(String, IndexEntry)>> {
        let _lock = self.lock_shared()?;
        let index = self.read_index(project)?;
        Ok(index.entries.into_iter().collect())
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
        if index.entries.remove(address).is_none() {
            return Ok(());
        }
        self.write_index(project, &index)
    }

    /// Every indexed target, as the status document `aster cache status` prints.
    pub fn load_status(&self) -> Result<StatusState> {
        let _lock = self.lock_shared()?;
        let mut state = StatusState::default();
        let dir = self.cache_dir.join("index");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(state),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", dir.display()))
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("Failed to read {}", dir.display()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let index = read_index_file(&path)?;
            for (address, row) in index.entries {
                state.targets.insert(
                    address,
                    StatusEntry {
                        hash: row.key,
                        timestamp: row.created_at,
                        success: true,
                    },
                );
            }
        }
        Ok(state)
    }

    /// Delete one address, or every address in a project (`//project`).
    ///
    /// Returns how many index entries were removed. Manifests are left for
    /// prune. Does not read `cache.json`.
    pub fn clear_matching(&self, pattern: &str) -> Result<usize> {
        if pattern.contains(':') {
            let before = self.load_status()?.targets.contains_key(pattern);
            self.remove(pattern)?;
            return Ok(usize::from(before));
        }
        let _lock = self.lock_exclusive()?;
        let mut removed = 0usize;
        let dir = self.cache_dir.join("index");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", dir.display()))
            }
        };
        let prefix = format!("{pattern}:");
        for entry in entries {
            let entry = entry.with_context(|| format!("Failed to read {}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let mut index = read_index_file(&path)?;
            let before = index.entries.len();
            index
                .entries
                .retain(|address, _| !address.starts_with(&prefix));
            let dropped = before - index.entries.len();
            if dropped == 0 {
                continue;
            }
            removed += dropped;
            write_index_file(&path, &index)?;
        }
        Ok(removed)
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
        }
    }

    #[test]
    fn lookup_requires_matching_index_key_and_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(tmp.path().to_path_buf());
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
        let cache = ArtifactCache::new(tmp.path().to_path_buf());
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
        let cache = ArtifactCache::new(tmp.path().to_path_buf());
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

        let status = cache.load_status().unwrap();
        assert_eq!(status.targets["//apps/web:build"].hash, "aa".repeat(32));
        assert!(status.targets["//apps/web:build"].success);

        let removed = cache.clear_matching("//apps/web").unwrap();
        assert_eq!(removed, 1);
        assert!(cache
            .load_status()
            .unwrap()
            .targets
            .contains_key("//apps/api:build"));
        assert!(!cache
            .load_status()
            .unwrap()
            .targets
            .contains_key("//apps/web:build"));
    }
}
