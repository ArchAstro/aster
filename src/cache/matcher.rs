//! Shared matcher for target input globs.
//!
//! Combines plugin-declared source globs + config files with user-declared
//! include/exclude patterns from aster.toml into a single compiled matcher.
//! Used by the cache hasher to pick files to hash, and by the watcher to
//! determine which target owns a changed path.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};

use crate::config::CacheConfig;
use crate::plugins::CacheInputs;

/// A catch-all project glob. It must not count as naming a tool-cache
/// directory, or `dist/` and `build/` could never be pruned.
fn is_broad_project_glob(pattern: &str) -> bool {
    matches!(pattern, "**/*" | "**" | "**/**" | "**/**/*")
}

/// Config files and user `include` patterns, without the catch-all glob.
///
/// A path inside a tool cache or a declared output is an input only when one
/// of these names it. `**/*` does not.
pub fn specific_input_patterns(
    plugin_inputs: &CacheInputs,
    user_config: Option<&CacheConfig>,
) -> Vec<String> {
    plugin_inputs
        .config_files
        .iter()
        .chain(user_config.iter().flat_map(|config| config.include.iter()))
        .filter(|pattern| !is_broad_project_glob(pattern))
        .cloned()
        .collect()
}

/// Whether `pattern` names `dir_rel` or a file inside it.
///
/// `dir_rel` uses `/` separators. A leading `**/` is stripped so
/// `**/.venv/pyvenv.cfg` still forces a walk of `.venv`.
pub fn pattern_names_cache_dir(pattern: &str, dir_rel: &str) -> bool {
    let pattern = pattern.strip_prefix("**/").unwrap_or(pattern);
    if is_broad_project_glob(pattern) {
        return false;
    }
    pattern == dir_rel || pattern.starts_with(&format!("{dir_rel}/"))
}

/// Directory name that must not be walked into a cache key.
///
/// Tooling directories are skipped at any depth. `target`, `_build`, `deps`,
/// `dist`, and `build` are skipped only as children of the project root, so
/// `src/build/widget.ts` and an embedded `internal/dist` stay inputs.
pub fn is_unhashed_cache_dir(name: &str, at_project_root: bool) -> bool {
    if matches!(
        name,
        ".git" | ".aster" | "node_modules" | "__pycache__" | ".venv" | ".pytest_cache"
    ) || name.ends_with(".egg-info")
    {
        return true;
    }
    at_project_root && matches!(name, "target" | "_build" | "deps" | "dist" | "build")
}

/// A file under a directory [`is_unhashed_cache_dir`] would skip.
///
/// A file whose own name is `build` is not a cache directory.
pub fn is_unhashed_cache_path(rel: &Path) -> bool {
    let mut components = rel.components();
    let Some(first) = components.next() else {
        return false;
    };
    let Some(first_name) = first.as_os_str().to_str() else {
        return false;
    };
    if components.next().is_some() && is_unhashed_cache_dir(first_name, true) {
        return true;
    }
    rel.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| is_unhashed_cache_dir(name, false))
    })
}

/// Compiled include/exclude globsets for a target, rooted at the project directory.
pub struct TargetInputMatcher {
    project_root: PathBuf,
    include: GlobSet,
    /// Config files and user includes. These can name a file inside a tool
    /// cache or a declared output; the catch-all source glob cannot.
    specific: GlobSet,
    exclude: GlobSet,
    outputs: GlobSet,
    has_patterns: bool,
}

impl TargetInputMatcher {
    /// Build a matcher from a plugin's cache inputs plus optional user overrides.
    pub fn build(
        project_root: &Path,
        plugin_inputs: &CacheInputs,
        user_config: Option<&CacheConfig>,
    ) -> Result<Self> {
        let mut include_builder = GlobSetBuilder::new();
        let mut has_patterns = false;

        for pattern in &plugin_inputs.source_globs {
            let glob = Glob::new(pattern)
                .with_context(|| format!("Invalid plugin cache input glob: {pattern}"))?;
            include_builder.add(glob);
            has_patterns = true;
        }
        for pattern in &plugin_inputs.config_files {
            let glob = Glob::new(pattern)
                .with_context(|| format!("Invalid plugin config-file glob: {pattern}"))?;
            include_builder.add(glob);
            has_patterns = true;
        }
        if let Some(cfg) = user_config {
            for pattern in &cfg.include {
                let glob = Glob::new(pattern)
                    .with_context(|| format!("Invalid cache include glob: {pattern}"))?;
                include_builder.add(glob);
                has_patterns = true;
            }
        }

        let include = include_builder
            .build()
            .context("Failed to build include globset")?;

        let mut specific_builder = GlobSetBuilder::new();
        for pattern in specific_input_patterns(plugin_inputs, user_config) {
            let glob = Glob::new(&pattern)
                .with_context(|| format!("Invalid cache input glob: {pattern}"))?;
            specific_builder.add(glob);
        }
        let specific = specific_builder
            .build()
            .context("Failed to build specific cache input globset")?;

        let mut exclude_builder = GlobSetBuilder::new();
        if let Some(cfg) = user_config {
            for pattern in &cfg.exclude {
                let glob = Glob::new(pattern)
                    .with_context(|| format!("Invalid cache exclude glob: {pattern}"))?;
                exclude_builder.add(glob);
            }
        }
        let exclude = exclude_builder
            .build()
            .context("Failed to build exclude globset")?;

        let mut output_builder = GlobSetBuilder::new();
        if let Some(outputs) = user_config.and_then(|config| config.outputs.as_ref()) {
            for pattern in outputs {
                let glob = Glob::new(pattern)
                    .with_context(|| format!("Invalid cache output glob: {pattern}"))?;
                output_builder.add(glob);
            }
        }
        let outputs = output_builder
            .build()
            .context("Failed to build cache output globset")?;

        Ok(Self {
            project_root: project_root.to_path_buf(),
            include,
            specific,
            exclude,
            outputs,
            has_patterns,
        })
    }

    /// Project root this matcher is bound to.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// True if the matcher has any include patterns.
    ///
    /// When false, callers know the target declared no inputs and must
    /// decide a fallback (cache: skip; watch: watch whole project dir).
    pub fn has_patterns(&self) -> bool {
        self.has_patterns
    }

    /// Check a project-relative path against the matcher.
    ///
    /// User `exclude` wins. A tool cache or declared output is not an input
    /// unless a config file or `include` names it. The catch-all `**/*` glob
    /// does not name those directories.
    pub fn matches_relative(&self, rel_path: &Path) -> bool {
        if !self.has_patterns {
            return false;
        }
        if !self.exclude.is_empty() && self.exclude.is_match(rel_path) {
            return false;
        }
        if is_unhashed_cache_path(rel_path) || self.outputs.is_match(rel_path) {
            return !self.specific.is_empty() && self.specific.is_match(rel_path);
        }
        self.include.is_match(rel_path)
    }

    /// Check an absolute path by stripping project_root first.
    ///
    /// Returns false if the path is outside the project.
    pub fn owns(&self, abs_path: &Path) -> bool {
        match abs_path.strip_prefix(&self.project_root) {
            Ok(rel) => self.matches_relative(rel),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(source: &[&str], config: &[&str]) -> CacheInputs {
        CacheInputs {
            source_globs: source.iter().map(|s| s.to_string()).collect(),
            config_files: config.iter().map(|s| s.to_string()).collect(),
            env_vars: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn has_patterns_false_for_empty_inputs() {
        let m = TargetInputMatcher::build(Path::new("/p"), &CacheInputs::default(), None).unwrap();
        assert!(!m.has_patterns());
        assert!(!m.matches_relative(Path::new("src/foo.ts")));
    }

    #[test]
    fn matches_source_globs() {
        let m = TargetInputMatcher::build(
            Path::new("/p"),
            &inputs(&["src/**/*.ts"], &["package.json"]),
            None,
        )
        .unwrap();
        assert!(m.has_patterns());
        assert!(m.matches_relative(Path::new("src/foo.ts")));
        assert!(m.matches_relative(Path::new("src/nested/bar.ts")));
        assert!(m.matches_relative(Path::new("package.json")));
        assert!(!m.matches_relative(Path::new("README.md")));
    }

    #[test]
    fn user_exclude_wins_over_include() {
        let user = CacheConfig {
            enabled: None,
            include: vec![],
            exclude: vec!["**/*.test.ts".to_string()],
            env: vec![],
            outputs: None,
            ..Default::default()
        };
        let m =
            TargetInputMatcher::build(Path::new("/p"), &inputs(&["src/**/*.ts"], &[]), Some(&user))
                .unwrap();
        assert!(m.matches_relative(Path::new("src/foo.ts")));
        assert!(!m.matches_relative(Path::new("src/foo.test.ts")));
    }

    #[test]
    fn user_include_extends_plugin_patterns() {
        let user = CacheConfig {
            enabled: None,
            include: vec!["extra/**/*.json".to_string()],
            exclude: vec![],
            env: vec![],
            outputs: None,
            ..Default::default()
        };
        let m =
            TargetInputMatcher::build(Path::new("/p"), &inputs(&["src/**/*.ts"], &[]), Some(&user))
                .unwrap();
        assert!(m.matches_relative(Path::new("src/foo.ts")));
        assert!(m.matches_relative(Path::new("extra/data.json")));
    }

    #[test]
    fn owns_strips_project_root() {
        let m = TargetInputMatcher::build(
            Path::new("/repo/libs/core"),
            &inputs(&["src/**/*.ts"], &[]),
            None,
        )
        .unwrap();
        assert!(m.owns(Path::new("/repo/libs/core/src/foo.ts")));
        assert!(!m.owns(Path::new("/repo/libs/core/README.md")));
        assert!(!m.owns(Path::new("/repo/libs/other/src/foo.ts")));
        assert!(!m.owns(Path::new("/somewhere/else/src/foo.ts")));
    }

    #[test]
    fn tool_cache_and_outputs_are_not_inputs_unless_named() {
        let user = CacheConfig {
            include: vec!["dist/keep.txt".to_string()],
            outputs: Some(vec!["bin/app".to_string(), "dist/**".to_string()]),
            ..Default::default()
        };
        let m = TargetInputMatcher::build(
            Path::new("/p"),
            &inputs(&["**/*"], &[".venv/pyvenv.cfg"]),
            Some(&user),
        )
        .unwrap();
        assert!(m.matches_relative(Path::new("src/build/widget.ts")));
        assert!(m.matches_relative(Path::new("internal/dist/asset.txt")));
        assert!(m.matches_relative(Path::new(".venv/pyvenv.cfg")));
        assert!(m.matches_relative(Path::new("dist/keep.txt")));
        assert!(!m.matches_relative(Path::new("build/out.js")));
        assert!(!m.matches_relative(Path::new("dist/other.txt")));
        assert!(!m.matches_relative(Path::new("bin/app")));
        assert!(!m.matches_relative(Path::new("node_modules/pkg/index.js")));
        assert!(!m.matches_relative(Path::new("src/__pycache__/x.pyc")));
    }

    #[test]
    fn user_exclude_wins_over_a_named_tool_cache_file() {
        let user = CacheConfig {
            include: vec!["dist/keep.txt".to_string()],
            exclude: vec!["dist/keep.txt".to_string()],
            ..Default::default()
        };
        let m = TargetInputMatcher::build(Path::new("/p"), &inputs(&["**/*"], &[]), Some(&user))
            .unwrap();
        assert!(!m.matches_relative(Path::new("dist/keep.txt")));
    }
}
