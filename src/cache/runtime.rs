//! Memoized runtime inputs for cache keys.
//!
//! A runtime input is a command whose trimmed stdout and stderr are hashed
//! into the cache key, the same idea as Nx runtime inputs. Each distinct
//! command runs at most once per Aster invocation, from the workspace root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::executor::command::parse_command;
use crate::executor::spawn_error_message;

/// Runs runtime-input commands once and remembers their output.
pub struct RuntimeInputCache {
    workspace_root: PathBuf,
    /// Command string to trimmed stdout+stderr, or the error from that run.
    results: Mutex<HashMap<String, Result<String, String>>>,
}

impl RuntimeInputCache {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            workspace_root: workspace_root.to_path_buf(),
            results: Mutex::new(HashMap::new()),
        }
    }

    /// Trimmed stdout+stderr of `command`, reusing a previous run of the same
    /// command string. The lock is held across the spawn so two targets cannot
    /// start the same command.
    pub fn output(&self, command: &str) -> Result<String, String> {
        let mut results = self
            .results
            .lock()
            .map_err(|_| "runtime input cache lock poisoned".to_string())?;
        if let Some(cached) = results.get(command) {
            return cached.clone();
        }
        let output = run_command(&self.workspace_root, command);
        results.insert(command.to_string(), output.clone());
        output
    }
}

fn run_command(workspace_root: &Path, command: &str) -> Result<String, String> {
    let parsed = parse_command(command).map_err(|error| format!("{error:#}"))?;
    let output = Command::new(&parsed.program)
        .args(&parsed.args)
        .envs(parsed.env)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            format!(
                "failed to run runtime input `{command}`: {}",
                spawn_error_message(&parsed.program, &error)
            )
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(if detail.is_empty() {
            format!("runtime input `{command}` exited {status}")
        } else {
            format!("runtime input `{command}` exited {status}: {detail}")
        });
    }
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&combined);
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_command_runs_once_for_two_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = tmp.path().join("runs");
        let script = tmp.path().join("count.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncount=0\nif [ -f runs ]; then count=$(cat runs); fi\ncount=$((count + 1))\necho \"$count\" > runs\necho v$count\n",
        )
        .unwrap();

        // Run through `sh` rather than exec'ing the freshly written file:
        // exec can fail with ETXTBSY while parallel tests fork.
        let cache = RuntimeInputCache::new(tmp.path());
        let command = format!("sh {}", script.display());
        let first = cache.output(&command).unwrap();
        let second = cache.output(&command).unwrap();

        assert_eq!(first, second);
        assert_eq!(first, "v1");
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "1");
    }

    #[test]
    fn missing_runtime_program_names_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = RuntimeInputCache::new(tmp.path());
        let error = cache
            .output("/aster-no-such-tool --version")
            .expect_err("missing program");
        assert!(
            error.contains("command not found: /aster-no-such-tool"),
            "{error}"
        );
        assert!(
            error.contains("failed to run runtime input `/aster-no-such-tool --version`"),
            "{error}"
        );
    }
}
