//! Hash computation for cache keys

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use walkdir::WalkDir;

use crate::cache::matcher::TargetInputMatcher;
use crate::config::CacheConfig;
use crate::plugins::CacheInputs;

/// Computes cache hashes for targets
pub struct CacheHasher {
    project_root: std::path::PathBuf,
}

impl CacheHasher {
    pub fn new(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
        }
    }

    /// Compute the cache key for a target
    ///
    /// The cache key is a SHA256 hash of:
    /// 1. Source files (matched by globs)
    /// 2. Config files
    /// 3. Command string
    /// 4. Dependency cache keys (hash chaining)
    /// 5. Environment variables
    pub fn compute_hash(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[&str],
        env_snapshot: &HashMap<String, String>,
    ) -> Result<String> {
        let mut hasher = Sha256::new();

        // 1. Hash source files
        let file_hashes = self.hash_files(plugin_inputs, user_config)?;
        for (path, hash) in &file_hashes {
            hasher.update(path.as_bytes());
            hasher.update(b":");
            hasher.update(hash.as_bytes());
            hasher.update(b"\n");
        }

        // 2. Hash command string
        hasher.update(b"command:");
        hasher.update(command.as_bytes());
        hasher.update(b"\n");

        // 3. Hash dependency cache keys (sorted for determinism)
        hasher.update(b"deps:");
        let mut sorted_deps: Vec<_> = dependency_hashes.iter().collect();
        sorted_deps.sort();
        for dep_hash in sorted_deps {
            hasher.update(dep_hash.as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\n");

        // 4. Hash environment variables
        hasher.update(b"env:");
        let env_vars = self.collect_env_vars(plugin_inputs, user_config);
        for var in &env_vars {
            if let Some(value) = env_snapshot.get(var) {
                hasher.update(var.as_bytes());
                hasher.update(b"=");
                hasher.update(value.as_bytes());
                hasher.update(b";");
            }
        }

        let result = hasher.finalize();
        Ok(format!("{result:x}"))
    }

    /// Hash all files matching the globs
    fn hash_files(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
    ) -> Result<Vec<(String, String)>> {
        let matcher = TargetInputMatcher::build(&self.project_root, plugin_inputs, user_config)?;
        if !matcher.has_patterns() {
            return Ok(Vec::new());
        }

        // Walk directory once and match files
        // Disable symlink following to prevent DoS via symlink cycles
        // and avoid reading files outside the project directory
        let mut file_hashes = Vec::new();

        for entry in WalkDir::new(&self.project_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let rel_path = match entry.path().strip_prefix(&self.project_root) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if matcher.matches_relative(rel_path) {
                if let Ok(content) = std::fs::read(entry.path()) {
                    let hash = format!("{:x}", Sha256::digest(&content));
                    file_hashes.push((rel_path.to_string_lossy().to_string(), hash));
                }
            }
        }

        // Sort for determinism
        file_hashes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(file_hashes)
    }

    /// Collect all env vars to track
    fn collect_env_vars(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
    ) -> BTreeSet<String> {
        let mut vars: BTreeSet<_> = plugin_inputs.env_vars.iter().cloned().collect();
        if let Some(config) = user_config {
            vars.extend(config.env.iter().cloned());
        }
        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_compute_hash_empty_inputs() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path());

        let inputs = CacheInputs::default();
        let env = HashMap::new();

        let hash = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();

        // Hash should be deterministic
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_compute_hash_different_commands() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path());

        let inputs = CacheInputs::default();
        let env = HashMap::new();

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm run test", &[], &env)
            .unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_hash_with_deps() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path());

        let inputs = CacheInputs::default();
        let env = HashMap::new();

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm test", &["abc123"], &env)
            .unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_hash_with_env() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path());

        let inputs = CacheInputs {
            env_vars: vec!["NODE_ENV".to_string()],
            ..Default::default()
        };

        let mut env1 = HashMap::new();
        env1.insert("NODE_ENV".to_string(), "test".to_string());

        let mut env2 = HashMap::new();
        env2.insert("NODE_ENV".to_string(), "production".to_string());

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env1)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env2)
            .unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_files() {
        let tmp = TempDir::new().unwrap();

        // Create some files
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.ts"), "console.log('hello')").unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();

        let hasher = CacheHasher::new(tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["src/**/*.ts".to_string()],
            config_files: vec!["package.json".to_string()],
            env_vars: vec![],
        };

        let file_hashes = hasher.hash_files(&inputs, None).unwrap();
        assert_eq!(file_hashes.len(), 2);

        // Verify paths are sorted
        assert!(file_hashes[0].0 < file_hashes[1].0 || file_hashes.len() == 1);
    }

    #[test]
    fn test_hash_files_with_exclude() {
        let tmp = TempDir::new().unwrap();

        // Create some files
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.ts"), "code").unwrap();
        std::fs::write(tmp.path().join("src/main.test.ts"), "test").unwrap();

        let hasher = CacheHasher::new(tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["src/**/*.ts".to_string()],
            config_files: vec![],
            env_vars: vec![],
        };
        let config = CacheConfig {
            enabled: None,
            include: vec![],
            exclude: vec!["**/*.test.ts".to_string()],
            env: vec![],
            outputs: vec![],
        };

        let file_hashes = hasher.hash_files(&inputs, Some(&config)).unwrap();
        assert_eq!(file_hashes.len(), 1);
        assert!(file_hashes[0].0.contains("main.ts"));
        assert!(!file_hashes[0].0.contains("test"));
    }
}
