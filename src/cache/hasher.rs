//! Hash computation for cache keys

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path};
use walkdir::WalkDir;

use crate::cache::matcher::TargetInputMatcher;
use crate::config::CacheConfig;
use crate::plugins::CacheInputs;

/// Relative path as hashed into a cache key: `Normal` components joined with
/// `/`, so Windows and Unix produce the same key. A non-UTF-8 component fails
/// the hash instead of becoming a lossy string.
fn normalize_rel(rel: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in rel.components() {
        if let Component::Normal(part) = component {
            let part = part.to_str().with_context(|| {
                format!("cache input path is not valid UTF-8: {}", rel.display())
            })?;
            parts.push(part);
        }
    }
    Ok(parts.join("/"))
}

/// Prefix for a workspace-relative path in a cache key.
///
/// A member project and the workspace root can both contain `Cargo.lock`.
/// Without a prefix those two paths hash as the same input.
const WORKSPACE_PATH_PREFIX: &str = "ws:";

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Computes cache hashes for targets
pub struct CacheHasher {
    project_root: std::path::PathBuf,
    workspace_root: std::path::PathBuf,
}

impl CacheHasher {
    /// `workspace_root` is where `workspace_files` and `workspace_include`
    /// are resolved. Pass the project root again for a project that is its
    /// own workspace.
    pub fn new(project_root: &Path, workspace_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            workspace_root: workspace_root.to_path_buf(),
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

        // 1. Hash source files, then workspace-root files. The `ws:` prefix
        // keeps a root Cargo.lock distinct from the member's own.
        let mut file_hashes = self.hash_files(plugin_inputs, user_config)?;
        file_hashes.extend(self.hash_workspace_files(plugin_inputs, user_config)?);
        file_hashes.sort_by(|a, b| a.0.cmp(&b.0));
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
        let env_vars = self.collect_env_vars(plugin_inputs, user_config, env_snapshot);
        for var in &env_vars {
            if let Some(value) = env_snapshot.get(var) {
                hasher.update(var.as_bytes());
                hasher.update(b"=");
                hasher.update(value.as_bytes());
                hasher.update(b";");
            }
        }

        let result = hasher.finalize();
        Ok(encode_hex(result.as_ref()))
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
                    let digest = Sha256::digest(&content);
                    let hash = encode_hex(digest.as_ref());
                    file_hashes.push((normalize_rel(rel_path)?, hash));
                }
            }
        }

        // Sort for determinism
        file_hashes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(file_hashes)
    }

    /// Hash workspace-root inputs.
    ///
    /// A pattern with no glob metacharacter is one file: read it directly so
    /// a root lockfile does not require walking the workspace. Anything else
    /// is a glob, matched by one walk. Missing files are omitted, matching
    /// how a missing project-relative config file is omitted.
    fn hash_workspace_files(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
    ) -> Result<Vec<(String, String)>> {
        let mut literals = Vec::new();
        let mut glob_builder = GlobSetBuilder::new();
        let mut has_globs = false;

        let mut add = |pattern: &str| -> Result<()> {
            if is_literal_path(pattern) {
                literals.push(pattern.to_string());
                return Ok(());
            }
            let glob = Glob::new(pattern)
                .with_context(|| format!("Invalid workspace cache glob: {pattern}"))?;
            glob_builder.add(glob);
            has_globs = true;
            Ok(())
        };

        for pattern in &plugin_inputs.workspace_files {
            add(pattern)?;
        }
        if let Some(config) = user_config {
            for pattern in &config.workspace_include {
                add(pattern)?;
            }
        }

        let mut file_hashes = Vec::new();
        let mut seen = BTreeSet::new();
        for pattern in literals {
            let rel = Path::new(&pattern);
            if !seen.insert(pattern.clone()) {
                continue;
            }
            if let Some(hash) = hash_file_contents(&self.workspace_root.join(rel))? {
                file_hashes.push((format!("{WORKSPACE_PATH_PREFIX}{pattern}"), hash));
            }
        }

        if has_globs {
            let globs = glob_builder
                .build()
                .context("Failed to build workspace cache globset")?;
            file_hashes.extend(self.walk_workspace_globs(&globs, &mut seen)?);
        }

        file_hashes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(file_hashes)
    }

    /// One walk of the workspace for every workspace glob.
    fn walk_workspace_globs(
        &self,
        globs: &GlobSet,
        seen: &mut BTreeSet<String>,
    ) -> Result<Vec<(String, String)>> {
        let mut file_hashes = Vec::new();
        for entry in WalkDir::new(&self.workspace_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let rel_path = match entry.path().strip_prefix(&self.workspace_root) {
                Ok(path) => path,
                Err(_) => continue,
            };
            if !globs.is_match(rel_path) {
                continue;
            }
            let rel = normalize_rel(rel_path)?;
            if !seen.insert(rel.clone()) {
                continue;
            }
            if let Some(hash) = hash_file_contents(entry.path())? {
                file_hashes.push((format!("{WORKSPACE_PATH_PREFIX}{rel}"), hash));
            }
        }
        Ok(file_hashes)
    }

    /// Collect all env vars to track.
    ///
    /// A name ending in `*` is a prefix: every snapshot key that starts with
    /// the name without that `*` is included. Exact names are kept as written.
    /// The set is sorted by name.
    fn collect_env_vars(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        env_snapshot: &HashMap<String, String>,
    ) -> BTreeSet<String> {
        let mut names: Vec<&str> = plugin_inputs.env_vars.iter().map(String::as_str).collect();
        if let Some(config) = user_config {
            names.extend(config.env.iter().map(String::as_str));
        }

        let mut vars = BTreeSet::new();
        for name in names {
            if let Some(prefix) = name.strip_suffix('*') {
                vars.extend(
                    env_snapshot
                        .keys()
                        .filter(|key| key.starts_with(prefix))
                        .cloned(),
                );
            } else {
                vars.insert(name.to_string());
            }
        }
        vars
    }
}

/// A workspace pattern with no glob metacharacter names one file.
fn is_literal_path(pattern: &str) -> bool {
    !pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | '{'))
}

/// Content hash of one file. `Ok(None)` when the file is absent, so a
/// toolchain file that only some workspaces have does not fail the key.
/// A present file that cannot be read is an error: skipping it would let
/// a permission failure look like "this input does not exist".
fn hash_file_contents(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(content) => {
            let digest = Sha256::digest(&content);
            Ok(Some(encode_hex(digest.as_ref())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn encode_hex_preserves_leading_zeroes_and_lowercase() {
        assert_eq!(encode_hex(&[0x00, 0x09, 0xaf, 0xff]), "0009afff");
    }

    #[test]
    fn test_compute_hash_empty_inputs() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

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
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

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
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

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
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

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
    fn test_env_prefix_pattern_hashes_matching_vars_in_order() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            env_vars: vec!["FOO_*".to_string(), "EXACT".to_string()],
            ..Default::default()
        };

        let mut env = HashMap::new();
        env.insert("FOO_B".to_string(), "b".to_string());
        env.insert("FOO_A".to_string(), "a".to_string());
        env.insert("BAR".to_string(), "no".to_string());
        env.insert("FOO".to_string(), "no-prefix".to_string());
        env.insert("EXACT".to_string(), "yes".to_string());

        let collected = hasher.collect_env_vars(&inputs, None, &env);
        assert_eq!(
            collected.iter().cloned().collect::<Vec<_>>(),
            vec!["EXACT", "FOO_A", "FOO_B"]
        );

        let hash = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &env)
            .unwrap();

        let mut bar_changed = env.clone();
        bar_changed.insert("BAR".to_string(), "changed".to_string());
        let hash_bar = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &bar_changed)
            .unwrap();
        assert_eq!(hash, hash_bar);

        let mut foo_changed = env.clone();
        foo_changed.insert("FOO_A".to_string(), "changed".to_string());
        let hash_foo = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &foo_changed)
            .unwrap();
        assert_ne!(hash, hash_foo);

        let mut swapped = HashMap::new();
        swapped.insert("FOO_A".to_string(), "b".to_string());
        swapped.insert("FOO_B".to_string(), "a".to_string());
        swapped.insert("EXACT".to_string(), "yes".to_string());
        let hash_swapped = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &swapped)
            .unwrap();
        assert_ne!(hash, hash_swapped);
    }

    #[test]
    fn test_user_env_prefix_pattern() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            env_vars: vec!["JAVA_HOME".to_string()],
            ..Default::default()
        };
        let config = CacheConfig {
            env: vec!["ORG_GRADLE_PROJECT_*".to_string(), "CI".to_string()],
            ..Default::default()
        };

        let mut env = HashMap::new();
        env.insert("JAVA_HOME".to_string(), "/usr/lib/jvm".to_string());
        env.insert("ORG_GRADLE_PROJECT_foo".to_string(), "1".to_string());
        env.insert("ORG_GRADLE_PROJECT_bar".to_string(), "2".to_string());
        env.insert("UNRELATED".to_string(), "no".to_string());

        let collected = hasher.collect_env_vars(&inputs, Some(&config), &env);
        assert_eq!(
            collected.iter().cloned().collect::<Vec<_>>(),
            vec![
                "CI",
                "JAVA_HOME",
                "ORG_GRADLE_PROJECT_bar",
                "ORG_GRADLE_PROJECT_foo",
            ]
        );

        let hash = hasher
            .compute_hash(&inputs, Some(&config), "gradle test", &[], &env)
            .unwrap();
        let mut changed = env.clone();
        changed.insert("ORG_GRADLE_PROJECT_foo".to_string(), "9".to_string());
        let hash_changed = hasher
            .compute_hash(&inputs, Some(&config), "gradle test", &[], &changed)
            .unwrap();
        assert_ne!(hash, hash_changed);

        let mut unrelated = env.clone();
        unrelated.insert("UNRELATED".to_string(), "yes".to_string());
        let hash_unrelated = hasher
            .compute_hash(&inputs, Some(&config), "gradle test", &[], &unrelated)
            .unwrap();
        assert_eq!(hash, hash_unrelated);
    }

    #[test]
    fn normalize_rel_uses_forward_slashes() {
        let rel = Path::new("a").join("b").join("c.rs");
        assert_eq!(normalize_rel(&rel).unwrap(), "a/b/c.rs");
    }

    #[test]
    fn hash_files_reports_nested_paths_with_forward_slashes() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("c.rs"), "fn main() {}").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["**/*.rs".to_string()],
            config_files: vec![],
            env_vars: vec![],
            ..Default::default()
        };

        let file_hashes = hasher.hash_files(&inputs, None).unwrap();
        let paths: Vec<_> = file_hashes.iter().map(|(path, _)| path.as_str()).collect();
        assert!(
            paths.contains(&"a/b/c.rs"),
            "sorted list should contain a/b/c.rs, got {paths:?}"
        );
        assert!(paths.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[cfg(unix)]
    #[test]
    fn normalize_rel_rejects_non_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let rel = Path::new(OsStr::from_bytes(b"a/\xff/c.rs"));
        let err = normalize_rel(rel).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("not valid UTF-8"),
            "error should reject the non-UTF-8 component, got {message}"
        );
    }

    #[test]
    fn test_hash_files() {
        let tmp = TempDir::new().unwrap();

        // Create some files
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.ts"), "console.log('hello')").unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["src/**/*.ts".to_string()],
            config_files: vec!["package.json".to_string()],
            env_vars: vec![],
            ..Default::default()
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

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["src/**/*.ts".to_string()],
            config_files: vec![],
            env_vars: vec![],
            ..Default::default()
        };
        let config = CacheConfig {
            enabled: None,
            include: vec![],
            exclude: vec!["**/*.test.ts".to_string()],
            env: vec![],
            outputs: vec![],
            ..Default::default()
        };

        let file_hashes = hasher.hash_files(&inputs, Some(&config)).unwrap();
        assert_eq!(file_hashes.len(), 1);
        assert!(file_hashes[0].0.contains("main.ts"));
        assert!(!file_hashes[0].0.contains("test"));
    }

    #[test]
    fn workspace_file_change_changes_member_key() {
        let tmp = TempDir::new().unwrap();
        let member = tmp.path().join("crates/app");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(member.join("src/lib.rs"), "fn app() {}").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "version = 3\n").unwrap();

        let hasher = CacheHasher::new(&member, tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["src/**/*.rs".to_string()],
            workspace_files: vec!["Cargo.lock".to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let before = hasher
            .compute_hash(&inputs, None, "cargo build", &[], &env)
            .unwrap();

        std::fs::write(tmp.path().join("Cargo.lock"), "version = 4\n").unwrap();
        let after = hasher
            .compute_hash(&inputs, None, "cargo build", &[], &env)
            .unwrap();

        assert_ne!(before, after);

        // The member's own Cargo.lock is a different input from the root one.
        std::fs::write(member.join("Cargo.lock"), "version = 4\n").unwrap();
        let inputs_with_member_lock = CacheInputs {
            config_files: vec!["Cargo.lock".to_string()],
            workspace_files: vec!["Cargo.lock".to_string()],
            ..Default::default()
        };
        let both = hasher
            .compute_hash(&inputs_with_member_lock, None, "cargo build", &[], &env)
            .unwrap();
        std::fs::write(member.join("Cargo.lock"), "version = 5\n").unwrap();
        let member_changed = hasher
            .compute_hash(&inputs_with_member_lock, None, "cargo build", &[], &env)
            .unwrap();
        assert_ne!(both, member_changed);
    }

    #[test]
    fn workspace_files_use_ws_prefix_and_user_globs() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("shared")).unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "lock\n").unwrap();
        std::fs::write(tmp.path().join("shared/base.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("shared/notes.md"), "skip\n").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            workspace_files: vec!["Cargo.lock".to_string(), "missing.toml".to_string()],
            ..Default::default()
        };
        let config = CacheConfig {
            workspace_include: vec!["shared/*.json".to_string()],
            ..Default::default()
        };

        let hashed = hasher.hash_workspace_files(&inputs, Some(&config)).unwrap();
        let paths: Vec<&str> = hashed.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, vec!["ws:Cargo.lock", "ws:shared/base.json"]);
    }
}
