//! Integration test submodules
//!
//! Contains additional integration tests organized by feature area.

pub mod json_output;

// Common test utilities shared across integration test modules
use std::fs;
use tempfile::TempDir;

/// Set up a minimal workspace with .git marker
pub fn setup_workspace(tmp: &TempDir) {
    fs::create_dir(tmp.path().join(".git")).unwrap();
}

/// Write a package.json file at the given path
pub fn write_package_json(tmp: &TempDir, path: &str, content: &str) {
    let full_path = tmp.path().join(path);
    fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    fs::write(full_path, content).unwrap();
}

/// Write any file at the given path within the temp directory
pub fn write_file(tmp: &TempDir, path: &str, content: &str) {
    let full_path = tmp.path().join(path);
    fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    fs::write(full_path, content).unwrap();
}
