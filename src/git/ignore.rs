//! Workspace-configured filtering for Git changes used by `aster affected`.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::AffectedWorkspaceConfig;

/// Aster's own state, workspace-relative. A run writes cache, locks, and logs
/// here, so they must not count as a change even when `.aster/` is not
/// gitignored. `**/.aster/**` covers a checkout nested under the git root.
const BUILTIN_IGNORE: &[&str] = &[".aster/**", "**/.aster/**"];

/// Compiled workspace-relative patterns excluded from affected analysis.
pub struct AffectedIgnore {
    builtin: GlobSet,
    configured: GlobSet,
}

impl AffectedIgnore {
    pub fn build(config: &AffectedWorkspaceConfig) -> Result<Self> {
        let mut builtin = GlobSetBuilder::new();
        for pattern in BUILTIN_IGNORE {
            builtin.add(
                Glob::new(pattern).with_context(|| {
                    format!("invalid built-in affected ignore pattern: {pattern}")
                })?,
            );
        }
        let builtin = builtin
            .build()
            .context("failed to build built-in affected ignore glob set")?;

        let mut configured = GlobSetBuilder::new();
        for pattern in &config.ignore {
            configured.add(
                Glob::new(pattern)
                    .with_context(|| format!("invalid affected.ignore pattern: {pattern}"))?,
            );
        }
        let configured = configured
            .build()
            .context("failed to build affected.ignore glob set")?;
        Ok(Self {
            builtin,
            configured,
        })
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        self.builtin.is_match(path) || self.configured.is_match(path)
    }

    /// A user `[affected] ignore` glob matches. Built-in `.aster/` drops are
    /// excluded so the verbose `affected.ignore` count stays accurate.
    pub fn is_configured_ignore(&self, path: &Path) -> bool {
        self.configured.is_match(path)
    }

    pub fn filter(&self, paths: HashSet<PathBuf>) -> HashSet<PathBuf> {
        paths
            .into_iter()
            .filter(|path| !self.is_ignored(path))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_ignored_paths_and_keeps_other_paths() {
        let config = AffectedWorkspaceConfig {
            ignore: vec![".agents/**".to_string(), "docs/generated/**".to_string()],
            ..AffectedWorkspaceConfig::default()
        };
        let ignore = AffectedIgnore::build(&config).unwrap();
        let paths = [
            PathBuf::from(".agents/skills/example/SKILL.md"),
            PathBuf::from("docs/generated/api.md"),
            PathBuf::from("src/main.rs"),
        ]
        .into_iter()
        .collect();

        let filtered = ignore.filter(paths);

        assert_eq!(filtered, HashSet::from([PathBuf::from("src/main.rs")]));
    }

    #[test]
    fn rejects_invalid_patterns() {
        let config = AffectedWorkspaceConfig {
            ignore: vec!["[".to_string()],
            ..AffectedWorkspaceConfig::default()
        };

        let error = AffectedIgnore::build(&config).err().unwrap();
        assert!(error
            .to_string()
            .contains("invalid affected.ignore pattern: ["));
    }

    #[test]
    fn drops_aster_state_and_keeps_configured_ignores() {
        let ignore = AffectedIgnore::build(&AffectedWorkspaceConfig::default()).unwrap();
        let paths = [
            PathBuf::from(".aster/cache/cache.lock"),
            PathBuf::from(".aster/logs/latest.json"),
            PathBuf::from(".aster/cache/related-facts/app.json"),
            PathBuf::from("services/api/.aster/cache/cache.lock"),
            PathBuf::from("src/main.rs"),
            PathBuf::from(".asterisk/readme.md"),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            ignore.filter(paths),
            HashSet::from([
                PathBuf::from("src/main.rs"),
                PathBuf::from(".asterisk/readme.md"),
            ])
        );
        assert!(!ignore.is_configured_ignore(Path::new(".aster/logs/latest.json")));

        let configured = AffectedWorkspaceConfig {
            ignore: vec![".agents/**".to_string()],
            ..AffectedWorkspaceConfig::default()
        };
        let ignore = AffectedIgnore::build(&configured).unwrap();
        assert!(ignore.is_ignored(Path::new(".agents/skills/example/SKILL.md")));
        assert!(ignore.is_configured_ignore(Path::new(".agents/skills/example/SKILL.md")));
        assert!(ignore.is_ignored(Path::new(".aster/cache/cache.lock")));
        assert!(!ignore.is_configured_ignore(Path::new(".aster/cache/cache.lock")));
        assert!(!ignore.is_ignored(Path::new("src/lib.rs")));
    }
}
