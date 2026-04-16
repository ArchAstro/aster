//! Target caching for incremental builds
//!
//! Caches target execution results based on input hashes to skip
//! re-running targets when nothing has changed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

mod hasher;
pub mod matcher;
mod store;

pub use hasher::CacheHasher;
pub use matcher::TargetInputMatcher;
pub use store::CacheStore;

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

/// Complete cache state stored in .aster/cache.json
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheState {
    /// Map of target address to cache entry
    pub targets: HashMap<String, CacheEntry>,
}
