//! Cache storage and retrieval

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use super::{CacheEntry, CacheState};

/// Manages the cache file at .aster/cache.json
pub struct CacheStore {
    cache_path: PathBuf,
    lock_path: PathBuf,
}

impl CacheStore {
    pub fn new(workspace_root: &Path) -> Self {
        let aster_dir = workspace_root.join(".aster");
        Self {
            cache_path: aster_dir.join("cache.json"),
            lock_path: aster_dir.join("cache.lock"),
        }
    }

    /// Acquire an exclusive lock on the cache file
    fn acquire_lock(&self) -> Result<File> {
        // Ensure directory exists
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.lock_path)
            .with_context(|| format!("Failed to open lock file {}", self.lock_path.display()))?;

        lock_file
            .lock_exclusive()
            .with_context(|| "Failed to acquire cache lock")?;

        Ok(lock_file)
    }

    /// Load the current cache state (without locking - use for read-only access)
    pub fn load(&self) -> Result<CacheState> {
        if !self.cache_path.exists() {
            return Ok(CacheState::default());
        }

        let content = fs::read_to_string(&self.cache_path)
            .with_context(|| format!("Failed to read {}", self.cache_path.display()))?;

        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", self.cache_path.display()))
    }

    /// Save the cache state (caller must hold lock)
    fn save_locked(&self, state: &CacheState) -> Result<()> {
        // Ensure directory exists
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(state)?;
        fs::write(&self.cache_path, content)
            .with_context(|| format!("Failed to write {}", self.cache_path.display()))?;

        Ok(())
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
        // Re-load state after acquiring lock to get latest version
        let mut state = self.load()?;
        state.targets.insert(address.to_string(), entry);
        self.save_locked(&state)
    }

    /// Remove cache entry for a target
    #[allow(dead_code)]
    pub fn remove(&self, address: &str) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load()?;
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
        let mut state = self.load()?;
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
        let store = CacheStore::new(tmp.path());

        let state = store.load().unwrap();
        assert!(state.targets.is_empty());
    }

    #[test]
    fn test_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(tmp.path());

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
        let store = CacheStore::new(tmp.path());

        let result = store.get("//nonexistent:target").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_clear() {
        let tmp = TempDir::new().unwrap();
        let store = CacheStore::new(tmp.path());

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
        let store = CacheStore::new(tmp.path());

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
        let store = CacheStore::new(tmp.path());

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
        let store = CacheStore::new(tmp.path());

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
}
