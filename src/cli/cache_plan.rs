//! `aster ci cache-plan`: actions/cache entries for native tool caches.
//!
//! Plugins describe caches; this command turns those descriptions into
//! concrete entries a workflow can hand to actions/cache. A workflow cannot
//! loop over a dynamic list of steps, so the plan also emits one combined
//! entry covering every path.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cache::RuntimeInputCache;
use crate::cli::output::{output_json, OutputMode};
use crate::discovery::DiscoveredProject;
use crate::plugins::{NativeCache, PluginRegistry, TargetContext};

/// One actions/cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachePlanEntry {
    /// Stable cache id from the plugin (`cargo-registry`, `node-modules:app`).
    pub id: String,
    /// Directories to save and restore. `~` is kept so the workflow expands
    /// it; `$(command)` paths are the command's trimmed output.
    pub paths: Vec<String>,
    /// `aster-l0-<id>-<os>-<arch>-<sha256>`.
    pub key: String,
    /// Prefix restore key, `aster-l0-<id>-<os>-<arch>-`.
    pub restore_keys: Vec<String>,
    /// Projects whose native cache collapsed into this entry.
    pub projects: Vec<String>,
}

/// The single entry a workflow restores and saves.
///
/// actions/cache cannot be invoked a dynamic number of times, so CI uses
/// this instead of looping over [`CachePlanEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CombinedCacheEntry {
    /// Every entry path, in first-seen order, without duplicates.
    pub paths: Vec<String>,
    /// `aster-l0-all-<os>-<arch>-<sha256 of the sorted entry keys>`.
    pub key: String,
    /// Prefix restore key, `aster-l0-all-<os>-<arch>-`.
    pub restore_keys: Vec<String>,
}

/// JSON body of `aster ci cache-plan --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachePlan {
    /// Per-cache entries, sorted by id.
    pub entries: Vec<CachePlanEntry>,
    /// One entry covering every path.
    pub combined: CombinedCacheEntry,
}

/// Build the plan for the selected projects.
///
/// Selection uses the same selectors as `aster run`. An empty selection
/// includes every discovered project; the caller applies that default.
pub fn plan_native_caches(
    selected: &[&DiscoveredProject],
    registry: &PluginRegistry,
    workspace_root: &Path,
    runtime: &RuntimeInputCache,
) -> Result<CachePlan> {
    let mut groups: BTreeMap<GroupKey, Group> = BTreeMap::new();

    for project in selected {
        let Some(plugin) = registry.find_by_name(&project.plugin_name) else {
            continue;
        };
        let ctx = TargetContext {
            config_path: &project.config_path,
            project_dir: &project.root,
            workspace_root,
            dependencies: &project.dependencies,
        };
        let address = project_address(project);
        for cache in plugin.native_caches(&ctx) {
            let paths = expand_paths(&cache, runtime)?;
            // Identity includes the key material. Two Go modules both name
            // `go-mod` at `~/go/pkg/mod`, but each hashes its own go.sum.
            // Merging those would make one module's lockfile miss the cache
            // for the other. Cargo members share one lockfile, so they still
            // collapse into one entry.
            let key_files: BTreeSet<String> = cache.key_files.iter().cloned().collect();
            let mut key_runtime = BTreeSet::new();
            if let Some(command) = &cache.key_runtime {
                key_runtime.insert(command.clone());
            }
            let key = (
                cache.id.clone(),
                paths.clone(),
                key_files.clone(),
                key_runtime.clone(),
            );
            let group = groups.entry(key).or_insert_with(|| Group {
                id: cache.id.clone(),
                paths,
                key_files,
                key_runtime,
                projects: BTreeSet::new(),
            });
            group.projects.insert(address.clone());
        }
    }

    let platform = host_platform();
    let mut entries = Vec::with_capacity(groups.len());
    for group in groups.into_values() {
        let digest = hash_key_material(
            workspace_root,
            &group.key_files,
            &group.key_runtime,
            runtime,
        )?;
        let prefix = format!("aster-l0-{}-{platform}-", group.id);
        entries.push(CachePlanEntry {
            id: group.id,
            paths: group.paths,
            key: format!("{prefix}{digest}"),
            restore_keys: vec![prefix],
            projects: group.projects.into_iter().collect(),
        });
    }

    Ok(CachePlan {
        combined: combined_entry(&entries, &platform),
        entries,
    })
}

/// Print the plan, and optionally append the combined entry to `$GITHUB_OUTPUT`.
pub fn render_cache_plan(plan: &CachePlan, mode: OutputMode, github_output: bool) -> Result<()> {
    if github_output {
        write_github_output(&plan.combined)?;
    }
    if mode == OutputMode::Json {
        output_json(plan)?;
    } else if mode != OutputMode::Quiet {
        print_table(plan);
    }
    Ok(())
}

type GroupKey = (String, Vec<String>, BTreeSet<String>, BTreeSet<String>);

struct Group {
    id: String,
    paths: Vec<String>,
    key_files: BTreeSet<String>,
    key_runtime: BTreeSet<String>,
    projects: BTreeSet<String>,
}

fn project_address(project: &DiscoveredProject) -> String {
    let relative = project.relative_path.display().to_string();
    if relative.is_empty() {
        "//".to_string()
    } else {
        format!("//{relative}")
    }
}

/// Expand `$(command)` paths. `~` stays so actions/cache expands it.
///
/// Two caches dedupe only when the resolved paths are identical.
fn expand_paths(cache: &NativeCache, runtime: &RuntimeInputCache) -> Result<Vec<String>> {
    let mut paths = Vec::with_capacity(cache.paths.len());
    for path in &cache.paths {
        paths.push(expand_path(path, runtime)?);
    }
    Ok(paths)
}

fn expand_path(path: &str, runtime: &RuntimeInputCache) -> Result<String> {
    let trimmed = path.trim();
    if let Some(command) = command_substitution(trimmed) {
        return runtime
            .output(command)
            .map_err(|error| anyhow::anyhow!("native cache path `{path}` failed: {error}"));
    }
    Ok(trimmed.to_string())
}

/// `$(go env GOCACHE)` → `go env GOCACHE`. Anything else is a literal path.
fn command_substitution(path: &str) -> Option<&str> {
    let inner = path.strip_prefix("$(")?.strip_suffix(')')?;
    if inner.contains("$(") || inner.contains(')') {
        return None;
    }
    let inner = inner.trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

fn host_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// sha256 of sorted `file:<rel>=<content-sha>` and `runtime:<cmd>=<output>`.
///
/// A missing key file is omitted. A present file that cannot be read, or a
/// runtime command that fails, fails the plan.
fn hash_key_material(
    workspace_root: &Path,
    key_files: &BTreeSet<String>,
    key_runtime: &BTreeSet<String>,
    runtime: &RuntimeInputCache,
) -> Result<String> {
    let mut parts = Vec::new();
    for pattern in key_files {
        for (rel, digest) in hash_key_pattern(workspace_root, pattern)? {
            parts.push(format!("file:{rel}={digest}"));
        }
    }
    for command in key_runtime {
        let output = runtime
            .output(command)
            .map_err(|error| anyhow::anyhow!("native cache runtime `{command}` failed: {error}"))?;
        parts.push(format!("runtime:{command}={output}"));
    }
    parts.sort();
    let mut hasher = Sha256::new();
    for part in &parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    Ok(encode_hex(hasher.finalize().as_ref()))
}

fn hash_key_pattern(workspace_root: &Path, pattern: &str) -> Result<Vec<(String, String)>> {
    if is_literal_path(pattern) {
        return match hash_file(workspace_root, pattern)? {
            Some(digest) => Ok(vec![(pattern.to_string(), digest)]),
            None => Ok(Vec::new()),
        };
    }
    let glob =
        Glob::new(pattern).with_context(|| format!("Invalid native cache key glob: {pattern}"))?;
    let globs = GlobSetBuilder::new()
        .add(glob)
        .build()
        .context("Failed to build native cache key globset")?;
    let mut matched = Vec::new();
    for entry in walkdir::WalkDir::new(workspace_root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let Ok(rel) = entry.path().strip_prefix(workspace_root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if globs.is_match(&rel) {
            if let Some(digest) = hash_file(workspace_root, &rel)? {
                matched.push((rel, digest));
            }
        }
    }
    matched.sort();
    Ok(matched)
}

fn hash_file(workspace_root: &Path, relative: &str) -> Result<Option<String>> {
    let path = workspace_root.join(relative);
    match std::fs::read(&path) {
        Ok(content) => {
            let digest = Sha256::digest(&content);
            Ok(Some(encode_hex(digest.as_ref())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn is_literal_path(pattern: &str) -> bool {
    !pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | '{'))
}

fn combined_entry(entries: &[CachePlanEntry], platform: &str) -> CombinedCacheEntry {
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        for path in &entry.paths {
            if seen.insert(path.clone()) {
                paths.push(path.clone());
            }
        }
    }
    let mut keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    keys.sort_unstable();
    let mut hasher = Sha256::new();
    for key in &keys {
        hasher.update(key.as_bytes());
        hasher.update(b"\n");
    }
    let digest = encode_hex(hasher.finalize().as_ref());
    let prefix = format!("aster-l0-all-{platform}-");
    CombinedCacheEntry {
        paths,
        key: format!("{prefix}{digest}"),
        restore_keys: vec![prefix],
    }
}

fn print_table(plan: &CachePlan) {
    println!("{:<28} {:<16} KEY", "ID", "PROJECTS");
    for entry in &plan.entries {
        println!(
            "{:<28} {:<16} {}",
            entry.id,
            entry.projects.len(),
            entry.key
        );
    }
    println!();
    println!("combined  {}", plan.combined.key);
    println!("paths     {}", plan.combined.paths.len());
}

/// Append the combined entry using the actions/cache output contract.
///
/// `paths` is a multiline heredoc. `restore-keys` is the hyphenated name
/// actions/cache reads; JSON keeps `restore_keys`.
fn write_github_output(combined: &CombinedCacheEntry) -> Result<()> {
    let path = std::env::var_os("GITHUB_OUTPUT")
        .context("--github-output requires GITHUB_OUTPUT to name the file actions/cache reads")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(path))
        .context("Failed to open $GITHUB_OUTPUT")?;
    let delimiter = github_delimiter(&combined.paths);
    writeln!(file, "paths<<{delimiter}")?;
    for path in &combined.paths {
        writeln!(file, "{path}")?;
    }
    writeln!(file, "{delimiter}")?;
    writeln!(file, "key={}", combined.key)?;
    writeln!(file, "restore-keys={}", combined.restore_keys.join("\n"))?;
    Ok(())
}

/// A heredoc delimiter that does not occur as a path line.
fn github_delimiter(paths: &[String]) -> String {
    let mut delimiter = "ASTER_CACHE_PATHS".to_string();
    while paths.iter().any(|path| path == &delimiter) {
        delimiter.push('_');
    }
    delimiter
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
