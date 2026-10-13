//! Cache storage and retrieval

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{CacheEntry, CacheState};

/// Manages `cache.json` and `cache.lock` inside a resolved cache directory.
pub struct CacheStore {
    cache_path: PathBuf,
    lock_path: PathBuf,
}

impl CacheStore {
    /// `cache_dir` is the directory that holds `cache.json` and `cache.lock`.
    /// Callers resolve it with [`super::resolve_cache_dir`].
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            cache_path: cache_dir.join("cache.json"),
            lock_path: cache_dir.join("cache.lock"),
        }
    }

    /// Directory that holds `cache.json` and `cache.lock`.
    pub fn cache_dir(&self) -> &Path {
        self.cache_path
            .parent()
            .expect("cache.json always has a parent directory")
    }

    /// Open `cache.lock`, creating the cache directory if needed.
    ///
    /// The file is not truncated: shared and exclusive holders share one inode,
    /// and truncating it would not release locks already held on the old inode.
    fn open_lock_file(&self) -> Result<File> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // truncate(false): the lock lives on this inode. Replacing it would
        // leave holders of the old inode unlocked against new openers.
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .with_context(|| format!("Failed to open lock file {}", self.lock_path.display()))
    }

    /// Acquire an exclusive lock on the cache file.
    fn acquire_lock(&self) -> Result<File> {
        let lock_file = self.open_lock_file()?;
        // UFCS: std::fs::File::lock_shared is 1.89; this crate's MSRV is 1.88.
        FileExt::lock_exclusive(&lock_file).with_context(|| "Failed to acquire cache lock")?;
        Ok(lock_file)
    }

    /// Acquire a shared lock so a reader cannot observe a half-written index.
    fn acquire_shared_lock(&self) -> Result<File> {
        let lock_file = self.open_lock_file()?;
        FileExt::lock_shared(&lock_file).with_context(|| "Failed to acquire shared cache lock")?;
        Ok(lock_file)
    }

    /// Load the current cache state, holding a shared lock for the read.
    pub fn load(&self) -> Result<CacheState> {
        let _lock = self.acquire_shared_lock()?;
        self.load_unlocked()
    }

    /// Load without taking a lock. Callers that already hold the exclusive
    /// lock use this so a process never waits on its own lock.
    fn load_unlocked(&self) -> Result<CacheState> {
        if !self.cache_path.exists() {
            return Ok(CacheState::default());
        }

        let content = fs::read_to_string(&self.cache_path)
            .with_context(|| format!("Failed to read {}", self.cache_path.display()))?;

        match serde_json::from_str(&content) {
            Ok(state) => Ok(state),
            Err(error) => {
                eprintln!(
                    "Warning: failed to parse {}, treating cache as empty: {error}",
                    self.cache_path.display()
                );
                Ok(CacheState::default())
            }
        }
    }

    /// Write `cache.json` via a same-directory temp file and rename.
    ///
    /// Rename is atomic on the same filesystem, so a concurrent reader never
    /// sees a partial index. The temp name includes pid and a counter so two
    /// writers in one process cannot clobber each other's temp file.
    fn write_atomic(&self, content: &str) -> Result<()> {
        static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

        let dir = self.cache_dir();
        fs::create_dir_all(dir)?;

        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dir.join(format!("cache.json.tmp.{}.{seq}", std::process::id()));

        let write_result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .with_context(|| format!("Failed to open {}", tmp_path.display()))?;
            file.write_all(content.as_bytes())
                .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("Failed to sync {}", tmp_path.display()))?;
            fs::rename(&tmp_path, &self.cache_path).with_context(|| {
                format!(
                    "Failed to rename {} to {}",
                    tmp_path.display(),
                    self.cache_path.display()
                )
            })
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        write_result
    }

    /// Save the cache state (caller must hold the exclusive lock).
    fn save_locked(&self, state: &CacheState) -> Result<()> {
        let content = serde_json::to_string_pretty(state)?;
        self.write_atomic(&content)
    }

    /// Save the cache state (public, acquires lock)
    pub fn save(&self, state: &CacheState) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.save_locked(state)
    }

    /// Get cache entry for a target
    pub fn get(&self, address: &str) -> Result<Option<CacheEntry>> {
        let state = self.load()?;
        Ok(state.targets.get(address).cloned())
    }

    /// Update cache entry for a target (thread-safe with file locking)
    pub fn set(&self, address: &str, entry: CacheEntry) -> Result<()> {
        let _lock = self.acquire_lock()?;
        // Re-load after acquiring the lock. load_unlocked avoids waiting on
        // the exclusive lock this process already holds.
        let mut state = self.load_unlocked()?;
        state.targets.insert(address.to_string(), entry);
        self.save_locked(&state)
    }

    /// Remove cache entry for a target
    #[allow(dead_code)]
    pub fn remove(&self, address: &str) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_unlocked()?;
        state.targets.remove(address);
        self.save_locked(&state)
    }

    /// Clear all cache entries
    pub fn clear(&self) -> Result<()> {
        let _lock = self.acquire_lock()?;
        self.save_locked(&CacheState::default())
    }

    /// Clear cache entries matching a pattern (supports //project:target or //project)
    pub fn clear_matching(&self, pattern: &str) -> Result<usize> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_unlocked()?;
        let before = state.targets.len();

        state.targets.retain(|addr, _| {
            if pattern.contains(':') {
                // Exact match
                addr != pattern
            } else {
                // Project prefix match
                !addr.starts_with(&format!("{pattern}:"))
            }
        });

        let removed = before - state.targets.len();
        self.save_locked(&state)?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_empty() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        let state = store.load().unwrap();
        assert!(state.targets.is_empty());
    }

    #[test]
    fn test_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        let entry = CacheEntry {
            hash: "abc123".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            success: true,
        };

        store.set("//test:build", entry.clone()).unwrap();

        let loaded = store.get("//test:build").unwrap().unwrap();
        assert_eq!(loaded.hash, "abc123");
        assert!(loaded.success);
    }

    #[test]
    fn test_get_missing() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        let result = store.get("//nonexistent:target").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_clear() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        let entry = CacheEntry {
            hash: "abc123".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            success: true,
        };

        store.set("//test:build", entry).unwrap();
        store.clear().unwrap();

        let state = store.load().unwrap();
        assert!(state.targets.is_empty());
    }

    #[test]
    fn test_clear_matching_exact() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        let entry = CacheEntry {
            hash: "abc123".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            success: true,
        };

        store.set("//test:build", entry.clone()).unwrap();
        store.set("//test:test", entry.clone()).unwrap();
        store.set("//other:build", entry).unwrap();

        let removed = store.clear_matching("//test:build").unwrap();
        assert_eq!(removed, 1);

        let state = store.load().unwrap();
        assert_eq!(state.targets.len(), 2);
        assert!(state.targets.contains_key("//test:test"));
        assert!(state.targets.contains_key("//other:build"));
    }

    #[test]
    fn test_clear_matching_project() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        let entry = CacheEntry {
            hash: "abc123".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            success: true,
        };

        store.set("//test:build", entry.clone()).unwrap();
        store.set("//test:test", entry.clone()).unwrap();
        store.set("//other:build", entry).unwrap();

        let removed = store.clear_matching("//test").unwrap();
        assert_eq!(removed, 2);

        let state = store.load().unwrap();
        assert_eq!(state.targets.len(), 1);
        assert!(state.targets.contains_key("//other:build"));
    }

    #[test]
    fn test_invalidate_project_clears_all_project_targets() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(&super::super::resolve_cache_dir(tmp.path()));

        // Set up cache entries for multiple projects
        let entry = CacheEntry {
            hash: "abc123".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            success: true,
        };

        store.set("//apps/web:build", entry.clone()).unwrap();
        store.set("//apps/web:test", entry.clone()).unwrap();
        store.set("//apps/web:lint", entry.clone()).unwrap();
        store.set("//libs/shared:build", entry.clone()).unwrap();
        store.set("//libs/shared:test", entry).unwrap();

        // Invalidate //apps/web
        let removed = store.clear_matching("//apps/web").unwrap();
        assert_eq!(removed, 3);

        // Verify only //libs/shared entries remain
        let state = store.load().unwrap();
        assert_eq!(state.targets.len(), 2);
        assert!(state.targets.contains_key("//libs/shared:build"));
        assert!(state.targets.contains_key("//libs/shared:test"));
        assert!(!state.targets.contains_key("//apps/web:build"));
    }

    #[test]
    fn test_concurrent_writers_leave_parseable_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let store = CacheStore::new(&root);

        let threads: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let store = CacheStore::new(&root);
                    for n in 0..20 {
                        let entry = CacheEntry {
                            hash: format!("hash-{i}-{n}"),
                            timestamp: "2024-01-01T00:00:00Z".to_string(),
                            success: true,
                        };
                        store.set(&format!("//test:t{i}"), entry).expect("set");
                        let _ = store.get(&format!("//test:t{i}")).expect("get");
                    }
                })
            })
            .collect();

        for thread in threads {
            thread.join().expect("writer thread");
        }

        let raw = fs::read_to_string(root.join("cache.json")).unwrap();
        let state: CacheState = serde_json::from_str(&raw).expect("index stays parseable");
        assert_eq!(state.targets.len(), 8);
        for i in 0..8 {
            let entry = state.targets.get(&format!("//test:t{i}")).unwrap();
            assert!(entry.hash.starts_with(&format!("hash-{i}-")));
        }

        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        // The public reader also sees a complete index.
        let loaded = store.load().unwrap();
        assert_eq!(loaded.targets.len(), 8);
    }

    #[test]
    fn test_corrupt_index_loads_as_empty() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(tmp.path());
        fs::write(tmp.path().join("cache.json"), "{not json").unwrap();

        let state = store.load().unwrap();
        assert!(state.targets.is_empty());

        let missing = store.get("//test:build").unwrap();
        assert!(missing.is_none());

        // A later write replaces the corrupt index instead of failing.
        store
            .set(
                "//test:build",
                CacheEntry {
                    hash: "recovered".to_string(),
                    timestamp: "2024-01-01T00:00:00Z".to_string(),
                    success: true,
                },
            )
            .unwrap();
        let loaded = store.get("//test:build").unwrap().unwrap();
        assert_eq!(loaded.hash, "recovered");
    }
}
