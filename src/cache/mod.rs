//! Target caching for incremental builds
//!
//! Caches target execution results based on input hashes to skip
//! re-running targets when nothing has changed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod artifact;
mod hasher;
mod local;
pub mod matcher;
mod prune;
mod runtime;
mod store;

pub use artifact::{Digest, Manifest, OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION};
pub use hasher::CacheHasher;
pub use local::{LocalStore, PruneStats};
pub use matcher::TargetInputMatcher;
pub use prune::{prune_cache, prune_if_over_cap, PruneLimits, DEFAULT_MAX_AGE, DEFAULT_MAX_SIZE};
pub use runtime::RuntimeInputCache;
pub use store::CacheStore;

/// Environment variable that overrides the cache directory.
///
/// A relative value is joined to the workspace root. An absolute value is
/// used as-is. `--cache-dir` is exported into this variable before any thread
/// or child process starts, so it is the single source of truth.
pub const CACHE_DIR_ENV: &str = "ASTER_CACHE_DIR";

/// Resolve the directory that holds `cache.json` and `cache.lock`.
///
/// Order: `ASTER_CACHE_DIR` (relative paths joined to `workspace_root`) >
/// `<workspace>/.aster/cache`.
pub fn resolve_cache_dir(workspace_root: &Path) -> PathBuf {
    match std::env::var_os(CACHE_DIR_ENV) {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        _ => workspace_root.join(".aster").join("cache"),
    }
}

/// Version mixed into every cache key by [`CacheHasher`].
///
/// Bump when the hashed material, its order, or the meaning of a key changes
/// so old entries cannot be restored under an incompatible layout. A crate
/// version change already salts keys; this constant covers layout changes
/// that ship without a version bump.
pub const CACHE_SCHEMA_VERSION: u32 = 2;

/// Entry in the cache for a single target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Hash of all inputs (files, config, command, deps, env)
    pub hash: String,
    /// When this entry was created (ISO 8601)
    pub timestamp: String,
    /// Whether the target succeeded on this run
    pub success: bool,
}

/// Complete cache state stored in the resolved cache directory's cache.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheState {
    /// Map of target address to cache entry
    pub targets: HashMap<String, CacheEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// `ASTER_CACHE_DIR` is process-global, so resolver tests share one lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn resolve_cache_dir_defaults_to_workspace_aster_cache() {
        let _guard = env_lock();
        let previous = std::env::var_os(CACHE_DIR_ENV);
        std::env::remove_var(CACHE_DIR_ENV);

        let workspace = Path::new("/workspace");
        assert_eq!(
            resolve_cache_dir(workspace),
            workspace.join(".aster").join("cache")
        );

        restore_env(previous);
    }

    #[test]
    fn resolve_cache_dir_joins_relative_env_to_workspace_root() {
        let _guard = env_lock();
        let previous = std::env::var_os(CACHE_DIR_ENV);
        std::env::set_var(CACHE_DIR_ENV, "shared/cache");

        let workspace = Path::new("/workspace");
        assert_eq!(
            resolve_cache_dir(workspace),
            workspace.join("shared").join("cache")
        );

        restore_env(previous);
    }

    #[test]
    fn resolve_cache_dir_keeps_absolute_env_path() {
        let _guard = env_lock();
        let previous = std::env::var_os(CACHE_DIR_ENV);
        let absolute = if cfg!(windows) {
            PathBuf::from(r"C:\tmp\aster-cache")
        } else {
            PathBuf::from("/tmp/aster-cache")
        };
        std::env::set_var(CACHE_DIR_ENV, &absolute);

        assert_eq!(resolve_cache_dir(Path::new("/workspace")), absolute);

        restore_env(previous);
    }

    fn restore_env(previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => std::env::set_var(CACHE_DIR_ENV, value),
            None => std::env::remove_var(CACHE_DIR_ENV),
        }
    }
}
