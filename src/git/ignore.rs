//! Workspace-configured filtering for Git changes used by `aster affected`.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::AffectedWorkspaceConfig;

/// Compiled workspace-relative patterns excluded from affected analysis.
pub struct AffectedIgnore {
    patterns: GlobSet,
}

impl AffectedIgnore {
    pub fn build(config: &AffectedWorkspaceConfig) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &config.ignore {
            builder.add(
                Glob::new(pattern)
                    .with_context(|| format!("invalid affected.ignore pattern: {pattern}"))?,
            );
        }

        let patterns = builder
            .build()
            .context("failed to build affected.ignore glob set")?;
        Ok(Self { patterns })
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        self.patterns.is_match(path)
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
}
