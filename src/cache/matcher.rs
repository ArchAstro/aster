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

/// Compiled include/exclude globsets for a target, rooted at the project directory.
pub struct TargetInputMatcher {
    project_root: PathBuf,
    include: GlobSet,
    exclude: GlobSet,
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
            if let Ok(glob) = Glob::new(pattern) {
                include_builder.add(glob);
                has_patterns = true;
            }
        }
        for pattern in &plugin_inputs.config_files {
            if let Ok(glob) = Glob::new(pattern) {
                include_builder.add(glob);
                has_patterns = true;
            }
        }
        if let Some(cfg) = user_config {
            for pattern in &cfg.include {
                if let Ok(glob) = Glob::new(pattern) {
                    include_builder.add(glob);
                    has_patterns = true;
                }
            }
        }

        let include = include_builder
            .build()
            .context("Failed to build include globset")?;

        let mut exclude_builder = GlobSetBuilder::new();
        if let Some(cfg) = user_config {
            for pattern in &cfg.exclude {
                if let Ok(glob) = Glob::new(pattern) {
                    exclude_builder.add(glob);
                }
            }
        }
        let exclude = exclude_builder
            .build()
            .context("Failed to build exclude globset")?;

        Ok(Self {
            project_root: project_root.to_path_buf(),
            include,
            exclude,
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
    pub fn matches_relative(&self, rel_path: &Path) -> bool {
        if !self.has_patterns {
            return false;
        }
        if !self.exclude.is_empty() && self.exclude.is_match(rel_path) {
            return false;
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
            include: vec![],
            exclude: vec!["**/*.test.ts".to_string()],
            env: vec![],
        };
        let m = TargetInputMatcher::build(
            Path::new("/p"),
            &inputs(&["src/**/*.ts"], &[]),
            Some(&user),
        )
        .unwrap();
        assert!(m.matches_relative(Path::new("src/foo.ts")));
        assert!(!m.matches_relative(Path::new("src/foo.test.ts")));
    }

    #[test]
    fn user_include_extends_plugin_patterns() {
        let user = CacheConfig {
            include: vec!["extra/**/*.json".to_string()],
            exclude: vec![],
            env: vec![],
        };
        let m = TargetInputMatcher::build(
            Path::new("/p"),
            &inputs(&["src/**/*.ts"], &[]),
            Some(&user),
        )
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
}
