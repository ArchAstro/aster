//! Hash computation for cache keys

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

use crate::cache::matcher::TargetInputMatcher;
use crate::cache::runtime::RuntimeInputCache;
use crate::cache::CACHE_SCHEMA_VERSION;
use crate::config::CacheConfig;
use crate::plugins::CacheInputs;

/// One file mixed into a cache key, in the order it is hashed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HashedFile {
    /// Path as written into the key (`ws:` prefixes a workspace-root file).
    pub path: String,
    /// SHA256 of the file contents.
    pub digest: String,
}

/// A dependency hash chained into this target's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyHash {
    /// Target address, when the caller supplied one. Hash-only callers leave
    /// this empty; the key still mixes only `hash`.
    pub address: String,
    /// That target's cache key. `None` when it has not been computed, so it
    /// is omitted from this key the same way a cold cache omits it.
    pub hash: Option<String>,
}

/// An environment variable named by the cache inputs.
///
/// `value_hash` is the SHA256 of the raw value. The raw value is never stored
/// here: status output must not print secrets. The cache key itself still
/// mixes the raw value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvInput {
    pub name: String,
    /// SHA256 of the value when the variable is set. `None` when unset.
    pub value_hash: Option<String>,
    /// Raw value, used only while hashing the key. Never serialized.
    #[serde(skip)]
    value: Option<String>,
}

/// One runtime command mixed into the key.
///
/// `output_hash` is the SHA256 of the trimmed stdout+stderr, for display.
/// The cache key mixes the raw output, kept here and never serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeInput {
    /// Command string, serialized as `cmd` to match status JSON.
    #[serde(rename = "cmd")]
    pub cmd: String,
    /// SHA256 of the trimmed stdout+stderr. `None` when the command failed;
    /// the key then records the error and hashing fails.
    pub output_hash: Option<String>,
    /// Failure text when the command did not produce output. Text status
    /// prints it; JSON status does not, so the documented shape stays stable.
    #[serde(skip)]
    pub error: Option<String>,
    /// Trimmed stdout+stderr. Hashed into the key; never printed.
    #[serde(skip)]
    output: Option<String>,
}

/// Every component of a target cache key, in hash order.
///
/// Built once and reused by [`CacheHasher::compute_hash`] and
/// `aster cache status --inputs`, so the printed key is the key execution
/// stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyMaterial {
    /// `aster-cache-v<schema>` plus the Aster version, the first two lines
    /// hashed into the key.
    pub salt: String,
    /// `platform:<os>-<arch>`.
    pub platform: String,
    /// Project files, then workspace files, sorted by path.
    pub files: Vec<HashedFile>,
    /// The target command string.
    pub command: String,
    /// Dependency keys. Only entries with a hash are mixed in, sorted by hash.
    pub deps: Vec<DependencyHash>,
    /// Tracked environment variables, sorted by name.
    pub env: Vec<EnvInput>,
    /// Path-valued environment variables, sorted by name. The hashed value is
    /// the portable form (workspace-relative path or final component), so a
    /// machine-absolute path never reaches the key.
    pub path_env: Vec<EnvInput>,
    /// Toolchain commands, in sorted command order.
    pub runtime: Vec<RuntimeInput>,
}

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

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Host OS and CPU architecture mixed into every cache key.
///
/// `std::env::consts` values, joined as `platform:<os>-<arch>` (for example
/// `platform:linux-x86_64`). A binary built on one host must not restore an
/// artifact produced on another.
fn host_platform() -> String {
    format!(
        "platform:{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Computes cache hashes for targets
pub struct CacheHasher {
    project_root: PathBuf,
    workspace_root: PathBuf,
    /// Nested project roots whose contents belong to another project and
    /// must not enter this project's cache key.
    excluded_dirs: Vec<PathBuf>,
    aster_version: String,
    /// `platform:<os>-<arch>`. Injectable so tests can prove a different
    /// host changes the key without running on that host.
    platform: String,
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
            platform: host_platform(),
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

    /// Hasher whose platform string is injected instead of the host OS and
    /// architecture. The project is its own workspace.
    #[cfg(test)]
    fn with_platform(project_root: &Path, platform: &str) -> Self {
        Self {
            platform: platform.to_string(),
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

    /// Compute the cache key for a target.
    ///
    /// Dependency hashes are mixed without addresses. Use
    /// [`Self::compute_hash_with_deps`] when status output needs each
    /// dependency's address next to its hash; both produce the same key.
    pub fn compute_hash(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[&str],
        env_snapshot: &HashMap<String, String>,
        runtime_inputs: &RuntimeInputCache,
    ) -> Result<String> {
        let deps: Vec<DependencyHash> = dependency_hashes
            .iter()
            .map(|hash| DependencyHash {
                address: String::new(),
                hash: Some((*hash).to_string()),
            })
            .collect();
        self.compute_hash_with_deps(
            plugin_inputs,
            user_config,
            command,
            &deps,
            env_snapshot,
            runtime_inputs,
        )
    }

    /// Compute the cache key, keeping each dependency address for display.
    ///
    /// A dependency with `hash: None` is listed but not mixed into the key.
    pub fn compute_hash_with_deps(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[DependencyHash],
        env_snapshot: &HashMap<String, String>,
        runtime_inputs: &RuntimeInputCache,
    ) -> Result<String> {
        self.material_and_hash(
            CACHE_SCHEMA_VERSION,
            plugin_inputs,
            user_config,
            command,
            dependency_hashes,
            env_snapshot,
            runtime_inputs,
        )
        .map(|(_material, key)| key)
        .map_err(cache_hash_error)
    }

    /// The components status prints, plus the key execution stores.
    ///
    /// A failed runtime command is still present on the material (with
    /// `output_hash: None` and `error` set). The error is returned after the
    /// material so status can show the failure and the partial key.
    pub fn explain(
        &self,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[DependencyHash],
        env_snapshot: &HashMap<String, String>,
        runtime_inputs: &RuntimeInputCache,
    ) -> (KeyMaterial, Result<String>) {
        match self.material_and_hash(
            CACHE_SCHEMA_VERSION,
            plugin_inputs,
            user_config,
            command,
            dependency_hashes,
            env_snapshot,
            runtime_inputs,
        ) {
            Ok((material, key)) => (material, Ok(key)),
            Err(CacheHashError::Runtime {
                material,
                partial,
                command,
                error,
            }) => (
                *material,
                Err(anyhow::anyhow!(
                    "runtime input `{command}` failed: {error} (inputs {partial})"
                )),
            ),
            Err(CacheHashError::Other(error)) => {
                let salt = format!(
                    "aster-cache-v{}\n{}",
                    CACHE_SCHEMA_VERSION, self.aster_version
                );
                (
                    KeyMaterial {
                        salt,
                        platform: self.platform.clone(),
                        files: Vec::new(),
                        command: command.to_string(),
                        deps: dependency_hashes.to_vec(),
                        env: Vec::new(),
                        path_env: Vec::new(),
                        runtime: Vec::new(),
                    },
                    Err(error),
                )
            }
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn compute_hash_with_schema(
        &self,
        schema: u32,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[&str],
        env_snapshot: &HashMap<String, String>,
        runtime_inputs: &RuntimeInputCache,
    ) -> Result<String> {
        let deps: Vec<DependencyHash> = dependency_hashes
            .iter()
            .map(|hash| DependencyHash {
                address: String::new(),
                hash: Some((*hash).to_string()),
            })
            .collect();
        self.material_and_hash(
            schema,
            plugin_inputs,
            user_config,
            command,
            &deps,
            env_snapshot,
            runtime_inputs,
        )
        .map(|(_material, key)| key)
        .map_err(cache_hash_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn material_and_hash(
        &self,
        schema: u32,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[DependencyHash],
        env_snapshot: &HashMap<String, String>,
        runtime_inputs: &RuntimeInputCache,
    ) -> std::result::Result<(KeyMaterial, String), CacheHashError> {
        let material = self.collect_key_material(
            schema,
            plugin_inputs,
            user_config,
            command,
            dependency_hashes,
            env_snapshot,
            runtime_inputs,
        )?;
        hash_key_material(&material)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_key_material(
        &self,
        schema: u32,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
        command: &str,
        dependency_hashes: &[DependencyHash],
        env_snapshot: &HashMap<String, String>,
        runtime_inputs: &RuntimeInputCache,
    ) -> std::result::Result<KeyMaterial, CacheHashError> {
        let mut files = self
            .hash_files(plugin_inputs, user_config)
            .map_err(CacheHashError::Other)?;
        files.extend(
            self.hash_workspace_files(plugin_inputs, user_config)
                .map_err(CacheHashError::Other)?,
        );
        files.sort_by(|a, b| a.0.cmp(&b.0));

        let env = self
            .collect_env_vars(plugin_inputs, user_config, env_snapshot)
            .into_iter()
            .map(|name| {
                let value = env_snapshot.get(&name).cloned();
                EnvInput {
                    value_hash: value.as_deref().map(hash_text),
                    value,
                    name,
                }
            })
            .collect();

        let path_env = self
            .collect_path_env_vars(plugin_inputs, env_snapshot)?
            .into_iter()
            .map(|(name, value)| EnvInput {
                value_hash: Some(hash_text(&value)),
                value: Some(value),
                name,
            })
            .collect();

        let mut runtime = Vec::new();
        for command in effective_runtime_commands(plugin_inputs, user_config) {
            match runtime_inputs.output(&command) {
                Ok(output) => runtime.push(RuntimeInput {
                    cmd: command,
                    output_hash: Some(hash_text(&output)),
                    error: None,
                    output: Some(output),
                }),
                Err(error) => runtime.push(RuntimeInput {
                    cmd: command,
                    output_hash: None,
                    error: Some(error),
                    output: None,
                }),
            }
        }

        Ok(KeyMaterial {
            salt: format!("aster-cache-v{schema}\n{}", self.aster_version),
            platform: self.platform.clone(),
            files: files
                .into_iter()
                .map(|(path, digest)| HashedFile { path, digest })
                .collect(),
            command: command.to_string(),
            deps: dependency_hashes.to_vec(),
            env,
            path_env,
            runtime,
        })
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

    /// Path-valued env vars, sorted by name, with a portable value.
    ///
    /// A value under the workspace root is the workspace-relative path.
    /// A value outside it is the final path component. An empty value stays
    /// empty. A missing variable is omitted, matching `env:`.
    fn collect_path_env_vars(
        &self,
        plugin_inputs: &CacheInputs,
        env_snapshot: &HashMap<String, String>,
    ) -> Result<Vec<(String, String)>> {
        let mut names: Vec<&str> = plugin_inputs
            .path_env_vars
            .iter()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();

        let mut vars = Vec::new();
        for name in names {
            let Some(value) = env_snapshot.get(name) else {
                continue;
            };
            vars.push((
                name.to_string(),
                portable_path_env(value, &self.workspace_root)?,
            ));
        }
        Ok(vars)
    }
}

/// Portable form of a path-valued env var.
///
/// `PYTHONPATH` is a list, so each entry is made portable and rejoined with
/// the platform separator. A single path (`JAVA_HOME`, `CARGO_TARGET_DIR`)
/// is one entry. Under `workspace_root`, an entry is the workspace-relative
/// path with `/` separators. Otherwise it is the final path component, so
/// two installs of `jdk-21` at different absolute paths share a key. The
/// toolchain version is a runtime input, not this path.
fn portable_path_env(value: &str, workspace_root: &Path) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let mut entries = Vec::new();
    for entry in std::env::split_paths(Path::new(value)) {
        entries.push(portable_path_entry(&entry, workspace_root)?);
    }
    Ok(entries.join(separator()))
}

fn separator() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

fn portable_path_entry(path: &Path, workspace_root: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(String::new());
    }
    if let Ok(rel) = path.strip_prefix(workspace_root) {
        if rel.as_os_str().is_empty() {
            return Ok(String::new());
        }
        return normalize_rel(rel);
    }
    path.file_name()
        .map(|name| {
            name.to_str()
                .map(str::to_string)
                .with_context(|| format!("path env value is not valid UTF-8: {}", path.display()))
        })
        .transpose()?
        .with_context(|| format!("path env value has no final component: {}", path.display()))
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

fn effective_runtime_commands(
    plugin_inputs: &CacheInputs,
    user_config: Option<&CacheConfig>,
) -> BTreeSet<String> {
    match user_config.and_then(|config| config.runtime.as_ref()) {
        Some(commands) => commands.iter().cloned().collect(),
        None => plugin_inputs.runtime_commands.iter().cloned().collect(),
    }
}

/// SHA256 of text, used for displayed env values and runtime outputs.
fn hash_text(text: &str) -> String {
    encode_hex(Sha256::digest(text.as_bytes()).as_ref())
}

fn cache_hash_error(error: CacheHashError) -> anyhow::Error {
    match error {
        CacheHashError::Runtime {
            partial,
            command,
            error,
            ..
        } => anyhow::anyhow!("runtime input `{command}` failed: {error} (inputs {partial})"),
        CacheHashError::Other(error) => error,
    }
}

/// A cache hash that failed after the inputs were collected, or before.
enum CacheHashError {
    /// A runtime command failed. `material` still lists every component,
    /// including the error, and `partial` is the key of what was hashed
    /// through that failure.
    Runtime {
        material: Box<KeyMaterial>,
        partial: String,
        command: String,
        error: String,
    },
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CacheHashError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

/// SHA256 of `material`, using the same byte layout `compute_hash` always has.
///
/// Schema and version are the two lines of `salt`. Platform follows. Files,
/// command, sorted dependency hashes, set env values, portable path-env
/// values, and runtime outputs
/// follow in that order. A runtime error is mixed as `runtime-error:` and
/// then returned with the partial digest, matching the previous hasher.
fn hash_key_material(
    material: &KeyMaterial,
) -> std::result::Result<(KeyMaterial, String), CacheHashError> {
    let mut hasher = Sha256::new();

    // Schema, Aster version, and host platform come first so incompatible
    // keys never collide. Platform is hashed immediately after the version
    // salt: a linux-x86_64 artifact must not restore on macOS-arm64.
    hasher.update(material.salt.as_bytes());
    hasher.update(b"\n");
    hasher.update(material.platform.as_bytes());
    hasher.update(b"\n");

    for file in &material.files {
        hasher.update(file.path.as_bytes());
        hasher.update(b":");
        hasher.update(file.digest.as_bytes());
        hasher.update(b"\n");
    }

    hasher.update(b"command:");
    hasher.update(material.command.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"deps:");
    let mut sorted_deps: Vec<&str> = material
        .deps
        .iter()
        .filter_map(|dep| dep.hash.as_deref())
        .collect();
    sorted_deps.sort_unstable();
    for dep_hash in sorted_deps {
        hasher.update(dep_hash.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\n");

    hasher.update(b"env:");
    for var in &material.env {
        if let Some(value) = &var.value {
            hasher.update(var.name.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b";");
        }
    }
    hasher.update(b"path-env:");
    for var in &material.path_env {
        if let Some(value) = &var.value {
            hasher.update(var.name.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b";");
        }
    }

    hasher.update(b"runtime:");
    for input in &material.runtime {
        if let Some(output) = &input.output {
            hasher.update(b"runtime:");
            hasher.update(input.cmd.as_bytes());
            hasher.update(b"=");
            hasher.update(output.as_bytes());
            hasher.update(b"\n");
            continue;
        }
        let error = input
            .error
            .clone()
            .unwrap_or_else(|| "runtime input failed".to_string());
        // Files are already in the hasher. Record the failure too so test
        // tracking still changes when an input file changes, then fail.
        hasher.update(b"runtime-error:");
        hasher.update(input.cmd.as_bytes());
        hasher.update(b"=");
        hasher.update(error.as_bytes());
        hasher.update(b"\n");
        let partial = encode_hex(hasher.finalize().as_ref());
        return Err(CacheHashError::Runtime {
            material: Box::new(material.clone()),
            partial,
            command: input.cmd.clone(),
            error,
        });
    }

    Ok((material.clone(), encode_hex(hasher.finalize().as_ref())))
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
    fn key_material_hash_matches_compute_hash() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{\"name\":\"app\"}\n").unwrap();
        std::fs::write(tmp.path().join("src.txt"), "source\n").unwrap();
        let script = tmp.path().join("ver.sh");
        std::fs::write(&script, "#!/bin/sh\necho toolchain\n").unwrap();

        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let mut inputs = CacheInputs::default();
        inputs.config_files.push("package.json".to_string());
        inputs.source_globs.push("src.txt".to_string());
        inputs.env_vars.push("APP_SECRET".to_string());
        inputs.env_vars.push("UNSET_VAR".to_string());
        inputs
            .runtime_commands
            .push(format!("sh {}", script.display()));
        let mut env = HashMap::new();
        env.insert("APP_SECRET".to_string(), "s3cret".to_string());
        let runtime = RuntimeInputCache::new(tmp.path());
        let deps = [
            DependencyHash {
                address: "//libs/core:build".to_string(),
                hash: Some("abc".to_string()),
            },
            DependencyHash {
                address: "//libs/missing:build".to_string(),
                hash: None,
            },
        ];

        let key = hasher
            .compute_hash(&inputs, None, "npm test", &["abc"], &env, &runtime)
            .unwrap();
        let (material, explained) =
            hasher.explain(&inputs, None, "npm test", &deps, &env, &runtime);
        assert_eq!(explained.unwrap(), key);
        assert!(material.salt.starts_with("aster-cache-v"));
        assert!(material.platform.starts_with("platform:"));
        assert!(material
            .files
            .iter()
            .any(|file| file.path == "package.json"));
        assert_eq!(material.command, "npm test");
        assert_eq!(material.deps[0].address, "//libs/core:build");
        assert!(material.deps[1].hash.is_none());
        let secret = material
            .env
            .iter()
            .find(|var| var.name == "APP_SECRET")
            .unwrap();
        assert_eq!(
            secret.value_hash.as_deref(),
            Some(hash_text("s3cret").as_str())
        );
        assert!(material
            .env
            .iter()
            .any(|var| var.name == "UNSET_VAR" && var.value_hash.is_none()));
        assert!(material.runtime[0].output_hash.is_some());

        let json = serde_json::to_value(&material).unwrap();
        let rendered = json.to_string();
        assert!(!rendered.contains("s3cret"));
        assert!(!rendered.contains("toolchain"));
        assert!(json.get("value").is_none());
    }

    #[test]
    fn test_compute_hash_empty_inputs() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

        let inputs = CacheInputs::default();
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let hash = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();

        // Hash should be deterministic
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_compute_hash_schema_version_changes_key() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs::default();
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let current = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let bumped = hasher
            .compute_hash_with_schema(
                CACHE_SCHEMA_VERSION + 1,
                &inputs,
                None,
                "npm test",
                &[],
                &env,
                &runtime,
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
                    &runtime,
                )
                .unwrap()
        );
    }

    #[test]
    fn test_compute_hash_aster_version_changes_key() {
        let tmp = TempDir::new().unwrap();
        let inputs = CacheInputs::default();
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let hash1 = CacheHasher::with_version(tmp.path(), "0.17.2")
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let hash2 = CacheHasher::with_version(tmp.path(), "0.18.0")
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_hash_platform_changes_key() {
        let tmp = TempDir::new().unwrap();
        let inputs = CacheInputs::default();
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let linux = CacheHasher::with_platform(tmp.path(), "platform:linux-x86_64")
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let macos = CacheHasher::with_platform(tmp.path(), "platform:macos-aarch64")
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let same = CacheHasher::with_platform(tmp.path(), "platform:linux-x86_64")
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();

        assert_ne!(linux, macos);
        assert_eq!(linux, same);
    }

    #[test]
    fn test_compute_hash_different_commands() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

        let inputs = CacheInputs::default();
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm run test", &[], &env, &runtime)
            .unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_hash_with_deps() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());

        let inputs = CacheInputs::default();
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm test", &["abc123"], &env, &runtime)
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
        let runtime = RuntimeInputCache::new(tmp.path());

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env1, &runtime)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env2, &runtime)
            .unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_env_prefix_pattern_hashes_matching_vars_in_order() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let runtime = RuntimeInputCache::new(tmp.path());
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
            .compute_hash(&inputs, None, "gradle test", &[], &env, &runtime)
            .unwrap();

        let mut bar_changed = env.clone();
        bar_changed.insert("BAR".to_string(), "changed".to_string());
        let hash_bar = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &bar_changed, &runtime)
            .unwrap();
        assert_eq!(hash, hash_bar);

        let mut foo_changed = env.clone();
        foo_changed.insert("FOO_A".to_string(), "changed".to_string());
        let hash_foo = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &foo_changed, &runtime)
            .unwrap();
        assert_ne!(hash, hash_foo);

        let mut swapped = HashMap::new();
        swapped.insert("FOO_A".to_string(), "b".to_string());
        swapped.insert("FOO_B".to_string(), "a".to_string());
        swapped.insert("EXACT".to_string(), "yes".to_string());
        let hash_swapped = hasher
            .compute_hash(&inputs, None, "gradle test", &[], &swapped, &runtime)
            .unwrap();
        assert_ne!(hash, hash_swapped);
    }

    #[test]
    fn test_user_env_prefix_pattern() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let runtime = RuntimeInputCache::new(tmp.path());
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
            .compute_hash(&inputs, Some(&config), "gradle test", &[], &env, &runtime)
            .unwrap();
        let mut changed = env.clone();
        changed.insert("ORG_GRADLE_PROJECT_foo".to_string(), "9".to_string());
        let hash_changed = hasher
            .compute_hash(
                &inputs,
                Some(&config),
                "gradle test",
                &[],
                &changed,
                &runtime,
            )
            .unwrap();
        assert_ne!(hash, hash_changed);

        let mut unrelated = env.clone();
        unrelated.insert("UNRELATED".to_string(), "yes".to_string());
        let hash_unrelated = hasher
            .compute_hash(
                &inputs,
                Some(&config),
                "gradle test",
                &[],
                &unrelated,
                &runtime,
            )
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
        let runtime = RuntimeInputCache::new(tmp.path());
        let before = hasher
            .compute_hash(&inputs, None, "cargo build", &[], &env, &runtime)
            .unwrap();

        std::fs::write(tmp.path().join("Cargo.lock"), "version = 4\n").unwrap();
        let after = hasher
            .compute_hash(&inputs, None, "cargo build", &[], &env, &runtime)
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
            .compute_hash(
                &inputs_with_member_lock,
                None,
                "cargo build",
                &[],
                &env,
                &runtime,
            )
            .unwrap();
        std::fs::write(member.join("Cargo.lock"), "version = 5\n").unwrap();
        let member_changed = hasher
            .compute_hash(
                &inputs_with_member_lock,
                None,
                "cargo build",
                &[],
                &env,
                &runtime,
            )
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

    #[test]
    fn runtime_command_output_changes_key() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            runtime_commands: vec!["echo v1".to_string()],
            ..Default::default()
        };
        let changed = CacheInputs {
            runtime_commands: vec!["echo v2".to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let hash1 = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let hash2 = hasher
            .compute_hash(&changed, None, "npm test", &[], &env, &runtime)
            .unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn user_runtime_replaces_plugin_defaults() {
        let tmp = TempDir::new().unwrap();
        let hasher = CacheHasher::new(tmp.path(), tmp.path());
        let inputs = CacheInputs {
            runtime_commands: vec!["echo plugin".to_string()],
            ..Default::default()
        };
        let replaced = CacheConfig {
            runtime: Some(vec!["echo user".to_string()]),
            ..Default::default()
        };
        let disabled = CacheConfig {
            runtime: Some(vec![]),
            ..Default::default()
        };
        let env = HashMap::new();
        let runtime = RuntimeInputCache::new(tmp.path());

        let plugin_hash = hasher
            .compute_hash(&inputs, None, "npm test", &[], &env, &runtime)
            .unwrap();
        let user_hash = hasher
            .compute_hash(&inputs, Some(&replaced), "npm test", &[], &env, &runtime)
            .unwrap();
        let disabled_hash = hasher
            .compute_hash(&inputs, Some(&disabled), "npm test", &[], &env, &runtime)
            .unwrap();
        let empty = CacheInputs::default();
        let no_runtime = hasher
            .compute_hash(&empty, None, "npm test", &[], &env, &runtime)
            .unwrap();

        assert_ne!(plugin_hash, user_hash);
        assert_ne!(plugin_hash, disabled_hash);
        assert_eq!(disabled_hash, no_runtime);
    }

    fn hash_path_env(workspace: &Path, value: &str) -> String {
        let hasher = CacheHasher::new(workspace, workspace);
        let inputs = CacheInputs {
            path_env_vars: vec!["JAVA_HOME".to_string()],
            ..Default::default()
        };
        let mut env = HashMap::new();
        env.insert("JAVA_HOME".to_string(), value.to_string());
        hasher
            .compute_hash(
                &inputs,
                None,
                "build",
                &[],
                &env,
                &RuntimeInputCache::new(workspace),
            )
            .unwrap()
    }

    #[test]
    fn path_env_under_workspace_is_hashed_relative() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let first_value = first.path().join("target").join("debug");
        let second_value = second.path().join("target").join("debug");
        std::fs::create_dir_all(&first_value).unwrap();
        std::fs::create_dir_all(&second_value).unwrap();

        let same = hash_path_env(first.path(), &first_value.to_string_lossy());
        let also_same = hash_path_env(second.path(), &second_value.to_string_lossy());
        assert_eq!(
            same, also_same,
            "the same relative path in two checkouts must share a key"
        );

        let other = first.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        let different = hash_path_env(first.path(), &other.to_string_lossy());
        assert_ne!(same, different);

        let collected = CacheHasher::new(first.path(), first.path()).collect_path_env_vars(
            &CacheInputs {
                path_env_vars: vec!["JAVA_HOME".to_string()],
                ..Default::default()
            },
            &HashMap::from([(
                "JAVA_HOME".to_string(),
                first_value.to_string_lossy().into_owned(),
            )]),
        );
        assert_eq!(
            collected.unwrap(),
            vec![("JAVA_HOME".to_string(), "target/debug".to_string())]
        );

        let listed = format!(
            "{}{}{}",
            first_value.display(),
            if cfg!(windows) { ";" } else { ":" },
            other.display()
        );
        let collected = CacheHasher::new(first.path(), first.path()).collect_path_env_vars(
            &CacheInputs {
                path_env_vars: vec!["PYTHONPATH".to_string()],
                ..Default::default()
            },
            &HashMap::from([("PYTHONPATH".to_string(), listed)]),
        );
        assert_eq!(
            collected.unwrap(),
            vec![(
                "PYTHONPATH".to_string(),
                format!(
                    "target/debug{}elsewhere",
                    if cfg!(windows) { ";" } else { ":" }
                )
            )]
        );
    }

    #[test]
    fn path_env_outside_workspace_is_hashed_as_basename() {
        let parent = TempDir::new().unwrap();
        let first = parent.path().join("checkout-a");
        let second = parent.path().join("checkout-b");
        let jdk_a = parent.path().join("opt").join("jdk-21");
        let jdk_b = parent.path().join("usr").join("lib").join("jdk-21");
        let jdk_17 = parent.path().join("opt").join("jdk-17");
        for dir in [&first, &second, &jdk_a, &jdk_b, &jdk_17] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let from_first = hash_path_env(&first, &jdk_a.to_string_lossy());
        let from_second = hash_path_env(&second, &jdk_b.to_string_lossy());
        assert_eq!(
            from_first, from_second,
            "same-version JDKs at different absolute paths must share a key"
        );

        let different = hash_path_env(&second, &jdk_17.to_string_lossy());
        assert_ne!(from_first, different);

        let collected = CacheHasher::new(&second, &second).collect_path_env_vars(
            &CacheInputs {
                path_env_vars: vec!["JAVA_HOME".to_string()],
                ..Default::default()
            },
            &HashMap::from([(
                "JAVA_HOME".to_string(),
                jdk_a.to_string_lossy().into_owned(),
            )]),
        );
        assert_eq!(
            collected.unwrap(),
            vec![("JAVA_HOME".to_string(), "jdk-21".to_string())]
        );
    }
}
