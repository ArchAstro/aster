//! Workspace-level fs-event ignore list.
//!
//! Defaults always apply (version control and common tooling output directories).
//! Users extend via `[watch].ignore` and `[watch].suppress_paths` in the root
//! `aster.toml`; patterns never replace the defaults.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

use crate::config::WatchWorkspaceConfig;

/// Patterns that are always ignored, even if the user doesn't configure anything.
pub const DEFAULT_IGNORE_GLOBS: &[&str] = &[
    "**/.git/**",
    "**/.git",
    "**/node_modules/**",
    "**/target/**",
    "**/_build/**",
    "**/dist/**",
    "**/.next/**",
    "**/.turbo/**",
    "**/.venv/**",
    "**/.elixir_ls/**",
    "**/.aster/**",
    "**/.DS_Store",
];

pub struct WorkspaceIgnore {
    ignore: GlobSet,
    suppress: GlobSet,
}

impl WorkspaceIgnore {
    pub fn build(cfg: &WatchWorkspaceConfig) -> Result<Self> {
        let mut ignore_b = GlobSetBuilder::new();
        for pat in DEFAULT_IGNORE_GLOBS {
            ignore_b.add(
                Glob::new(pat).with_context(|| format!("invalid default ignore pattern: {pat}"))?,
            );
        }
        for pat in &cfg.ignore {
            ignore_b.add(
                Glob::new(pat).with_context(|| format!("invalid watch.ignore pattern: {pat}"))?,
            );
        }
        let ignore = ignore_b.build().context("failed to build ignore globset")?;

        let mut suppress_b = GlobSetBuilder::new();
        for pat in &cfg.suppress_paths {
            suppress_b.add(
                Glob::new(pat)
                    .with_context(|| format!("invalid watch.suppress_paths pattern: {pat}"))?,
            );
        }
        let suppress = suppress_b
            .build()
            .context("failed to build suppress globset")?;

        Ok(Self { ignore, suppress })
    }

    /// Check a workspace-relative path.
    pub fn is_ignored(&self, rel: &Path) -> bool {
        self.ignore.is_match(rel)
    }

    pub fn is_suppressed(&self, rel: &Path) -> bool {
        self.suppress.is_match(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_default() -> WorkspaceIgnore {
        WorkspaceIgnore::build(&WatchWorkspaceConfig::default()).unwrap()
    }

    #[test]
    fn defaults_ignore_git_and_node_modules() {
        let wi = build_default();
        assert!(wi.is_ignored(Path::new("services/api/node_modules/foo/index.js")));
        assert!(wi.is_ignored(Path::new(".git/HEAD")));
        assert!(wi.is_ignored(Path::new("libs/core/target/debug/app")));
        assert!(wi.is_ignored(Path::new("services/platform/_build/dev/lib/x.beam")));
        assert!(!wi.is_ignored(Path::new("services/api/src/main.ts")));
    }

    #[test]
    fn user_ignore_extends_defaults() {
        let cfg = WatchWorkspaceConfig {
            ignore: vec!["coverage/**".to_string()],
            ..Default::default()
        };
        let wi = WorkspaceIgnore::build(&cfg).unwrap();
        assert!(wi.is_ignored(Path::new("coverage/lcov.info")));
        assert!(wi.is_ignored(Path::new(".git/index")));
        assert!(!wi.is_ignored(Path::new("src/main.ts")));
    }

    #[test]
    fn suppress_paths_match_separately() {
        let cfg = WatchWorkspaceConfig {
            suppress_paths: vec!["services/platform/priv/static/assets/**".to_string()],
            ..Default::default()
        };
        let wi = WorkspaceIgnore::build(&cfg).unwrap();
        assert!(wi.is_suppressed(Path::new("services/platform/priv/static/assets/app.js")));
        assert!(!wi.is_suppressed(Path::new("services/platform/lib/router.ex")));
    }
}
