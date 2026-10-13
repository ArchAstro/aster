//! Hash computation for cache keys

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

use crate::cache::matcher::TargetInputMatcher;
use crate::cache::CACHE_SCHEMA_VERSION;
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
    project_root: PathBuf,
    workspace_root: PathBuf,
    /// Nested project roots whose contents belong to another project and
    /// must not enter this project's cache key.
    excluded_dirs: Vec<PathBuf>,
    aster_version: String,
}

impl CacheHasher {
    /// `workspace_root` is where `workspace_files` and `workspace_include`
    /// are resolved. Pass the project root again for a project that is its
    /// own workspace.
    pub fn new(project_root: &Path, workspace_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            workspace_root: workspace_root.to_path_buf(),
            excluded_dirs: Vec::new(),
            aster_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Hasher whose version salt is injected instead of the crate version.
    /// The project is its own workspace.
    #[cfg(test)]
    fn with_version(project_root: &Path, version: &str) -> Self {
        Self {
            aster_version: version.to_string(),
            ..Self::new(project_root, project_root)
        }
    }

    /// Skip these directories while hashing. Each path is a directory under
    /// `project_root` (absolute, or relative to it) that is itself another
    /// Aster project. The directory is not descended into, so a glob that
    /// would otherwise match a nested project's files does not hash them.
    pub fn with_excluded_dirs(mut self, excluded_dirs: Vec<PathBuf>) -> Self {
        self.excluded_dirs = excluded_dirs;
        self
    }

    /// Compute the cache key for a target
    ///
    /// The cache key is a SHA256 hash of:
    /// 1. Cache schema (`aster-cache-v<SCHEMA>`) and Aster version
    /// 2. Source files (matched by globs)
    /// 3. Config files
    /// 4. Command string
    /// 5. Dependency cache keys (hash chaining)
    /// 6. Environment variables
    pub fn compute_hash(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[&str],
        env_snapshot: &HashMap<String, String>,
    ) -> Result<String> {
        self.compute_hash_with_schema(
            CACHE_SCHEMA_VERSION,
            plugin_inputs,
            user_config,
            command,
            dependency_hashes,
            env_snapshot,
        )
    }

    fn compute_hash_with_schema(
        &self,
        schema: u32,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[&str],
        env_snapshot: &HashMap<String, String>,
    ) -> Result<String> {
        let mut hasher = Sha256::new();

        // Schema and Aster version come first so incompatible keys never collide.
        hasher.update(format!("aster-cache-v{schema}").as_bytes());
        hasher.update(b"\n");
        hasher.update(self.aster_version.as_bytes());
        hasher.update(b"\n");

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

        // Walk once. The ignore crate never visits gitignored directories
        // (node_modules, target, .venv), so a matching glob cannot pull them
        // into the key. Symlinks are not followed: a cycle or a link outside
        // the project must not be hashed. Hidden files stay visible because
        // a glob can name them on purpose. Nested project roots are pruned by
        // their path relative to the root: the walker joins entry names onto
        // the root exactly as given, so comparing relative paths works whether
        // the root is canonical, a symlink, or a platform alias like /var.
        let excluded = self.excluded_relative_dirs();
        let root = self.project_root.clone();
        let mut file_hashes = Vec::new();

        let walker = WalkBuilder::new(&self.project_root)
            .hidden(false)
            .follow_links(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .require_git(false)
            .filter_entry(move |entry| {
                let Ok(rel) = entry.path().strip_prefix(&root) else {
                    return true;
                };
                !excluded.iter().any(|dir| rel.starts_with(dir))
            })
            .build();

        for entry in walker.filter_map(Result::ok).filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        }) {
            let rel_path = match entry.path().strip_prefix(&self.project_root) {
                Ok(path) => path,
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

    /// Excluded directories as non-empty paths relative to `project_root`.
    ///
    /// Each directory may be given relative to the root, or absolute in the
    /// root's own spelling or in canonical form (for example `/private/var`
    /// on macOS when the root is spelled `/var`). Directories that are not
    /// under the root are dropped.
    fn excluded_relative_dirs(&self) -> Vec<PathBuf> {
        let canonical_root = self.project_root.canonicalize().ok();
        self.excluded_dirs
            .iter()
            .filter_map(|dir| {
                if dir.is_relative() {
                    return Some(dir.clone());
                }
                if let Ok(rel) = dir.strip_prefix(&self.project_root) {
                    return Some(rel.to_path_buf());
                }
                let canonical_root = canonical_root.as_ref()?;
                let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
                canonical_dir
                    .strip_prefix(canonical_root)
                    .ok()
                    .map(Path::to_path_buf)
            })
            .filter(|rel| !rel.as_os_str().is_empty())
            .collect()
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
    fn test_compute_hash_schema_version_changes_key() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs::default();
        let env = HashMap::new();

        let current = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();
        let bumped = hasher
            .compute_hash_with_schema(
                CACHE_SCHEMA_VERSION + 1,
                &inputs,
                None,
                "npm test",
                &[],
                &env,
            )
            .unwrap();

        assert_ne!(current, bumped);
        assert_eq!(
            current,
            hasher
                .compute_hash_with_schema(
                    CACHE_SCHEMA_VERSION,
                    &inputs,
                    None,
                    "npm test",
                    &[],
                    &env,
                )
                .unwrap()
        );
    }

    #[test]
    fn test_compute_hash_aster_version_changes_key() {
        let tmp = TempDir::new().unwrap();
        let inputs = CacheInputs::default();
        let env = HashMap::new();

        let hash1 = CacheHasher::with_version(tmp.path(), "0.17.2")
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();
        let hash2 = CacheHasher::with_version(tmp.path(), "0.18.0")
            .compute_hash(&inputs, None, "npm test", &[], &env)
            .unwrap();

        assert_ne!(hash1, hash2);
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

    #[test]
    fn test_hash_files_skips_gitignored_dirs() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
        std::fs::write(tmp.path().join("src/app.js"), "app").unwrap();
        std::fs::write(tmp.path().join("node_modules/pkg/x.js"), "vendor").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["**/*.js".to_string()],
            config_files: vec![],
            env_vars: vec![],
            ..Default::default()
        };

        let before = hasher.hash_files(&inputs, None).unwrap();
        assert_eq!(before.len(), 1);
        assert!(before[0].0.ends_with("src/app.js") || before[0].0.ends_with("src\\app.js"));

        std::fs::write(tmp.path().join("node_modules/pkg/x.js"), "changed vendor").unwrap();
        let after = hasher.hash_files(&inputs, None).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn test_hash_files_honors_ignore_file_and_git_exclude() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
        std::fs::write(tmp.path().join(".ignore"), "generated/\n").unwrap();
        std::fs::write(tmp.path().join(".git/info/exclude"), "scratch/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("generated")).unwrap();
        std::fs::create_dir_all(tmp.path().join("scratch")).unwrap();
        std::fs::write(tmp.path().join("src/app.js"), "app").unwrap();
        std::fs::write(tmp.path().join("generated/out.js"), "generated").unwrap();
        std::fs::write(tmp.path().join("scratch/note.js"), "scratch").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["**/*.js".to_string()],
            config_files: vec![],
            env_vars: vec![],
            ..Default::default()
        };

        let hashes = hasher.hash_files(&inputs, None).unwrap();
        assert_eq!(
            hashes.len(),
            1,
            "ignored trees must not be hashed: {hashes:?}"
        );
        assert!(hashes[0].0.contains("src"));
    }

    #[test]
    fn test_hash_files_includes_hidden_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1").unwrap();
        std::fs::write(tmp.path().join("visible.env"), "SECRET=1").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec![],
            config_files: vec!["**/*.env".to_string(), ".env".to_string()],
            env_vars: vec![],
            ..Default::default()
        };

        let hashes = hasher.hash_files(&inputs, None).unwrap();
        let paths: Vec<&str> = hashes.iter().map(|(path, _)| path.as_str()).collect();
        assert!(
            paths
                .iter()
                .any(|path| path == &".env" || path.ends_with("/.env")),
            "hidden files must still be hashed when a glob matches: {paths:?}"
        );
    }

    #[test]
    fn test_hash_files_does_not_follow_symlinks() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.js"), "outside").unwrap();
        std::fs::write(tmp.path().join("app.js"), "inside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), tmp.path().join("linked")).unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            source_globs: vec!["**/*.js".to_string()],
            config_files: vec![],
            env_vars: vec![],
            ..Default::default()
        };

        let hashes = hasher.hash_files(&inputs, None).unwrap();
        assert!(
            hashes.iter().all(|(path, _)| !path.contains("secret.js")),
            "symlink targets outside the project must not be hashed: {hashes:?}"
        );
        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn test_hash_files_skips_excluded_nested_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("sub/package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("sub/a.js"), "nested").unwrap();
        std::fs::write(tmp.path().join("app.js"), "root").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path())
            .with_excluded_dirs(vec![tmp.path().join("sub")]);
        let inputs = CacheInputs {
            source_globs: vec!["**/*.js".to_string(), "sub/*.js".to_string()],
            config_files: vec!["**/package.json".to_string()],
            env_vars: vec![],
            ..Default::default()
        };

        let unexcluded = CacheHasher::new(tmp.path(), tmp.path())
            .hash_files(&inputs, None)
            .unwrap();
        assert!(
            unexcluded.iter().any(|(path, _)| path.contains("a.js")),
            "the glob must match the nested file, or this test proves nothing: {unexcluded:?}"
        );

        let before = hasher.hash_files(&inputs, None).unwrap();
        assert!(
            before.iter().all(|(path, _)| !path.contains("sub")),
            "nested project files must not be hashed: {before:?}"
        );

        std::fs::write(tmp.path().join("sub/a.js"), "changed nested").unwrap();
        let after = hasher.hash_files(&inputs, None).unwrap();
        assert_eq!(before, after);
    }

    #[cfg(unix)]
    #[test]
    fn test_hash_files_skips_excluded_nested_project_through_aliased_root() {
        // macOS temp dirs live under /var, a symlink to /private/var. Reach
        // the project through a symlink so this runs the same way everywhere,
        // and name the excluded dir in both the alias and canonical forms.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("app.js"), "root").unwrap();
        std::fs::write(real.join("sub/a.js"), "nested").unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let inputs = CacheInputs {
            source_globs: vec!["**/*.js".to_string()],
            config_files: vec![],
            env_vars: vec![],
            ..Default::default()
        };
        for excluded in [alias.join("sub"), real.canonicalize().unwrap().join("sub")] {
            let hashes = CacheHasher::new(&alias, &alias)
                .with_excluded_dirs(vec![excluded.clone()])
                .hash_files(&inputs, None)
                .unwrap();
            assert_eq!(
                hashes
                    .iter()
                    .map(|(path, _)| path.as_str())
                    .collect::<Vec<_>>(),
                vec!["app.js"],
                "excluding {excluded:?} through an aliased root"
            );
        }
    }
}
