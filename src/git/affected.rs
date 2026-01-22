//! Git change detection using git2
//!
//! Detects files changed between git refs and uncommitted changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{DiffOptions, Repository, StatusOptions};

/// Detector for files affected by git changes
pub struct AffectedDetector {
    repo: Repository,
}

impl AffectedDetector {
    /// Create a new AffectedDetector from a workspace root
    ///
    /// Uses Repository::discover() to find the .git directory,
    /// searching upward from workspace_root.
    pub fn new(workspace_root: &Path) -> Result<Self> {
        let repo = Repository::discover(workspace_root)
            .context("Not in a git repository. The 'affected' command requires git.")?;
        Ok(Self { repo })
    }

    /// Get the repository workdir (root of the working tree)
    pub fn workdir(&self) -> Option<&Path> {
        self.repo.workdir()
    }

    /// Get files changed between two git refs
    ///
    /// Returns paths relative to the repository root.
    pub fn changed_files_between_refs(
        &self,
        base: &str,
        head: &str,
    ) -> Result<HashSet<PathBuf>> {
        let base_obj = self
            .repo
            .revparse_single(base)
            .with_context(|| format!("Git ref '{}' not found. Check your --base value.", base))?;

        let head_obj = self
            .repo
            .revparse_single(head)
            .with_context(|| format!("Git ref '{}' not found. Check your --head value.", head))?;

        let base_tree = base_obj
            .peel_to_tree()
            .with_context(|| format!("Could not get tree for ref '{}'", base))?;

        let head_tree = head_obj
            .peel_to_tree()
            .with_context(|| format!("Could not get tree for ref '{}'", head))?;

        let mut opts = DiffOptions::new();
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut opts))
            .context("Failed to compute diff between refs")?;

        let mut changed = HashSet::new();

        for delta in diff.deltas() {
            // Include both old and new paths (handles renames)
            if let Some(old_path) = delta.old_file().path() {
                changed.insert(old_path.to_path_buf());
            }
            if let Some(new_path) = delta.new_file().path() {
                changed.insert(new_path.to_path_buf());
            }
        }

        Ok(changed)
    }

    /// Get uncommitted changes (staged, unstaged, and untracked)
    ///
    /// Returns paths relative to the repository root.
    pub fn uncommitted_changes(&self) -> Result<HashSet<PathBuf>> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .include_ignored(false)
            .include_unmodified(false);

        let statuses = self
            .repo
            .statuses(Some(&mut opts))
            .context("Failed to get git status")?;

        let mut changed = HashSet::new();

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                changed.insert(PathBuf::from(path));
            }
        }

        Ok(changed)
    }

    /// Get all affected files based on base ref and optional head ref
    ///
    /// - If head is provided: only compare refs (no uncommitted changes)
    /// - If head is None: compare base..HEAD plus uncommitted changes
    ///
    /// This matches Nx default behavior where uncommitted changes are included
    /// unless an explicit head ref is specified.
    pub fn all_affected_files(
        &self,
        base: &str,
        head: Option<&str>,
    ) -> Result<HashSet<PathBuf>> {
        match head {
            Some(head_ref) => {
                // Only compare committed changes between refs
                self.changed_files_between_refs(base, head_ref)
            }
            None => {
                // Compare base..HEAD plus uncommitted changes
                let mut files = self.changed_files_between_refs(base, "HEAD")?;
                let uncommitted = self.uncommitted_changes()?;
                files.extend(uncommitted);
                Ok(files)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use std::process::Command;

    /// Helper to create a git repo with initial commit
    fn setup_git_repo(tmp: &TempDir) {
        let path = tmp.path();

        // Initialize repo
        Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();

        // Configure git user for commits
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .unwrap();

        // Create initial file and commit
        fs::write(path.join("README.md"), "# Test").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(path)
            .output()
            .unwrap();
    }

    #[test]
    fn test_new_fails_outside_git_repo() {
        let tmp = TempDir::new().unwrap();
        // No git init, just empty directory
        let result = AffectedDetector::new(tmp.path());
        assert!(result.is_err());
        let err = format!("{:#}", result.err().unwrap());
        assert!(err.contains("git repository") || err.contains("Git"));
    }

    #[test]
    fn test_new_succeeds_in_git_repo() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);
        let result = AffectedDetector::new(tmp.path());
        assert!(result.is_ok());
    }

    #[test]
    fn test_uncommitted_changes_detects_new_file() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);

        // Add untracked file
        fs::write(tmp.path().join("new_file.txt"), "content").unwrap();

        let detector = AffectedDetector::new(tmp.path()).unwrap();
        let changes = detector.uncommitted_changes().unwrap();

        assert!(changes.contains(&PathBuf::from("new_file.txt")));
    }

    #[test]
    fn test_uncommitted_changes_detects_modified_file() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);

        // Modify tracked file
        fs::write(tmp.path().join("README.md"), "# Modified").unwrap();

        let detector = AffectedDetector::new(tmp.path()).unwrap();
        let changes = detector.uncommitted_changes().unwrap();

        assert!(changes.contains(&PathBuf::from("README.md")));
    }

    #[test]
    fn test_changed_files_between_refs() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);
        let path = tmp.path();

        // Create a new file and commit
        fs::write(path.join("feature.txt"), "feature").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Add feature"])
            .current_dir(path)
            .output()
            .unwrap();

        let detector = AffectedDetector::new(path).unwrap();
        let changes = detector
            .changed_files_between_refs("HEAD~1", "HEAD")
            .unwrap();

        assert!(changes.contains(&PathBuf::from("feature.txt")));
        assert!(!changes.contains(&PathBuf::from("README.md")));
    }

    #[test]
    fn test_all_affected_files_with_head() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);
        let path = tmp.path();

        // Create a commit with a file
        fs::write(path.join("feature.txt"), "feature").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Add feature"])
            .current_dir(path)
            .output()
            .unwrap();

        // Add uncommitted file
        fs::write(path.join("uncommitted.txt"), "uncommitted").unwrap();

        let detector = AffectedDetector::new(path).unwrap();

        // With explicit head, should NOT include uncommitted
        let changes = detector
            .all_affected_files("HEAD~1", Some("HEAD"))
            .unwrap();

        assert!(changes.contains(&PathBuf::from("feature.txt")));
        assert!(!changes.contains(&PathBuf::from("uncommitted.txt")));
    }

    #[test]
    fn test_all_affected_files_without_head() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);
        let path = tmp.path();

        // Create a commit with a file
        fs::write(path.join("feature.txt"), "feature").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Add feature"])
            .current_dir(path)
            .output()
            .unwrap();

        // Add uncommitted file
        fs::write(path.join("uncommitted.txt"), "uncommitted").unwrap();

        let detector = AffectedDetector::new(path).unwrap();

        // Without explicit head, should include uncommitted
        let changes = detector.all_affected_files("HEAD~1", None).unwrap();

        assert!(changes.contains(&PathBuf::from("feature.txt")));
        assert!(changes.contains(&PathBuf::from("uncommitted.txt")));
    }

    #[test]
    fn test_invalid_ref_gives_clear_error() {
        let tmp = TempDir::new().unwrap();
        setup_git_repo(&tmp);

        let detector = AffectedDetector::new(tmp.path()).unwrap();
        let result = detector.changed_files_between_refs("nonexistent", "HEAD");

        assert!(result.is_err());
        let err = format!("{:#}", result.err().unwrap());
        assert!(err.contains("nonexistent") || err.contains("not found"));
    }
}
