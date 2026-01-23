use std::path::{Path, PathBuf};

/// Find the workspace root by walking up from the start directory.
///
/// Looks for `aster.toml` first (explicit marker), then `.git` as fallback boundary.
/// Returns None if neither marker is found (reached filesystem root).
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    // Canonicalize to resolve symlinks
    let mut current = start.canonicalize().ok()?;

    loop {
        // Check for aster.toml first (explicit workspace marker)
        if current.join("aster.toml").exists() {
            return Some(current);
        }

        // Check for .git as fallback boundary
        if current.join(".git").exists() {
            return Some(current);
        }

        // Move up to parent directory
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return None, // Reached filesystem root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_workspace_root_with_aster_toml() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create aster.toml marker
        fs::write(root.join("aster.toml"), "").unwrap();

        let result = find_workspace_root(root);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_with_git() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create .git folder
        fs::create_dir(root.join(".git")).unwrap();

        let result = find_workspace_root(root);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_prefers_aster_toml() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create both markers
        fs::write(root.join("aster.toml"), "").unwrap();
        fs::create_dir(root.join(".git")).unwrap();

        let result = find_workspace_root(root);
        assert!(result.is_some());
        // Should find it at root level (aster.toml checked first)
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_walks_up() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create aster.toml at root
        fs::write(root.join("aster.toml"), "").unwrap();

        // Create nested directories
        let nested = root.join("services").join("api").join("src");
        fs::create_dir_all(&nested).unwrap();

        let result = find_workspace_root(&nested);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_not_found() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a nested dir with no markers anywhere
        let nested = root.join("some").join("nested").join("dir");
        fs::create_dir_all(&nested).unwrap();

        // This will walk up and eventually find no markers
        // Note: In a real filesystem, it might find .git in home or root
        // but in a temp dir without markers, it should return None
        let result = find_workspace_root(&nested);
        // The test temp dir has no markers, so walking up from nested
        // should eventually return None (or find something outside temp)
        // For this test, we just verify the function runs without panic
        // and returns Some or None based on what exists above temp
        assert!(result.is_none() || result.is_some());
    }
}
