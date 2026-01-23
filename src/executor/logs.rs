//! Log storage for execution results
//!
//! Stores execution logs at .aster/logs/latest.json for later retrieval
//! via `aster logs` command.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Complete log of a single execution run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLog {
    /// ISO 8601 timestamp when run started
    pub timestamp: String,
    /// Target that was executed (e.g., "test")
    pub target: String,
    /// Results for each target that was run
    pub results: Vec<TargetLog>,
}

/// Log of a single target execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetLog {
    /// Target address (e.g., "//services/api:test")
    pub address: String,
    /// Execution status ("passed", "failed", "skipped")
    pub status: String,
    /// Exit code if available (None for skipped targets)
    pub exit_code: Option<i32>,
    /// Execution duration in milliseconds
    pub duration_ms: u128,
    /// Full stdout+stderr output
    pub output: String,
}

/// Store for persisting and retrieving execution logs
pub struct LogStore {
    /// Directory for log files (.aster/logs/)
    log_dir: PathBuf,
}

impl LogStore {
    /// Create a new LogStore for the given workspace
    pub fn new(workspace_root: &Path) -> Self {
        let log_dir = workspace_root.join(".aster").join("logs");
        Self { log_dir }
    }

    /// Store a run log to latest.json
    ///
    /// Creates the log directory if needed. Overwrites any existing latest.json.
    pub fn store(&self, run: &RunLog) -> anyhow::Result<()> {
        fs::create_dir_all(&self.log_dir)?;

        let latest_path = self.log_dir.join("latest.json");
        let json = serde_json::to_string_pretty(run)?;
        fs::write(&latest_path, json)?;

        Ok(())
    }

    /// Load the latest run log if it exists
    pub fn load_latest(&self) -> anyhow::Result<Option<RunLog>> {
        let latest_path = self.log_dir.join("latest.json");

        if !latest_path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&latest_path)?;
        let run: RunLog = serde_json::from_str(&contents)?;
        Ok(Some(run))
    }

    /// Get the log for a specific target from the latest run
    pub fn get_target_log(&self, address: &str) -> anyhow::Result<Option<TargetLog>> {
        let run = match self.load_latest()? {
            Some(r) => r,
            None => return Ok(None),
        };

        Ok(run.results.into_iter().find(|t| t.address == address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_store_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = LogStore::new(tmp.path());

        let run = RunLog {
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            target: "test".to_string(),
            results: vec![
                TargetLog {
                    address: "//a:test".to_string(),
                    status: "passed".to_string(),
                    exit_code: Some(0),
                    duration_ms: 100,
                    output: "test output".to_string(),
                },
                TargetLog {
                    address: "//b:test".to_string(),
                    status: "failed".to_string(),
                    exit_code: Some(1),
                    duration_ms: 50,
                    output: "error message".to_string(),
                },
            ],
        };

        store.store(&run).unwrap();

        let loaded = store.load_latest().unwrap().unwrap();
        assert_eq!(loaded.timestamp, "2024-01-01T00:00:00Z");
        assert_eq!(loaded.target, "test");
        assert_eq!(loaded.results.len(), 2);
        assert_eq!(loaded.results[0].address, "//a:test");
        assert_eq!(loaded.results[1].status, "failed");
    }

    #[test]
    fn test_load_latest_not_found() {
        let tmp = TempDir::new().unwrap();
        let store = LogStore::new(tmp.path());

        let result = store.load_latest().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_target_log() {
        let tmp = TempDir::new().unwrap();
        let store = LogStore::new(tmp.path());

        let run = RunLog {
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            target: "test".to_string(),
            results: vec![
                TargetLog {
                    address: "//a:test".to_string(),
                    status: "passed".to_string(),
                    exit_code: Some(0),
                    duration_ms: 100,
                    output: "output a".to_string(),
                },
                TargetLog {
                    address: "//b:test".to_string(),
                    status: "failed".to_string(),
                    exit_code: Some(1),
                    duration_ms: 50,
                    output: "output b".to_string(),
                },
            ],
        };

        store.store(&run).unwrap();

        let target = store.get_target_log("//a:test").unwrap().unwrap();
        assert_eq!(target.address, "//a:test");
        assert_eq!(target.output, "output a");

        let target_b = store.get_target_log("//b:test").unwrap().unwrap();
        assert_eq!(target_b.status, "failed");

        let not_found = store.get_target_log("//c:test").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn test_get_target_log_no_logs() {
        let tmp = TempDir::new().unwrap();
        let store = LogStore::new(tmp.path());

        let result = store.get_target_log("//a:test").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_log_serialization_roundtrip() {
        let log = TargetLog {
            address: "//services/api:test".to_string(),
            status: "passed".to_string(),
            exit_code: Some(0),
            duration_ms: 1500,
            output: "All tests passed\n".to_string(),
        };

        let json = serde_json::to_string(&log).unwrap();
        let restored: TargetLog = serde_json::from_str(&json).unwrap();

        assert_eq!(log.address, restored.address);
        assert_eq!(log.status, restored.status);
        assert_eq!(log.exit_code, restored.exit_code);
        assert_eq!(log.duration_ms, restored.duration_ms);
        assert_eq!(log.output, restored.output);
    }
}
