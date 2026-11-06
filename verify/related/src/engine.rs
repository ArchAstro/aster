//! Runs `aster affected test --related` over a scratch git repo and reads
//! back which test files it selects.

use crate::render::{self, PROJECT};
use crate::repo::{Changes, Repo};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// What the engine decided for the project's `test` target.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// The whole project runs, not a list of files.
    pub full: bool,
    /// Test files the plan names, relative to the project (`test/t7_test.exs`).
    pub files: BTreeSet<String>,
    /// The engine's reasons, for diagnostics.
    pub notes: String,
}

impl Plan {
    /// The test units a model repo's plan names (the `N` of `test/tN_test.exs`).
    pub fn units(&self) -> Result<BTreeSet<i64>, String> {
        self.files
            .iter()
            .map(|file| {
                file.trim_start_matches("test/t")
                    .trim_end_matches("_test.exs")
                    .parse()
                    .map_err(|_| format!("odd test file {file}"))
            })
            .collect()
    }
}

/// The `aster` binary under test: `ASTER_BIN`, or the one built from this checkout.
pub fn aster_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ASTER_BIN") {
        return Ok(PathBuf::from(path));
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let binary = root.join("target/debug/aster");
    if !binary.exists() {
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["build", "--locked", "--bin", "aster", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .status()
            .map_err(|error| format!("cannot build aster: {error}"))?;
        if !status.success() {
            return Err("cargo build --bin aster failed".to_string());
        }
    }
    Ok(binary)
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("{} {args:?}: {error}", program.display()))
}

fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
    // The identity is passed per command; nothing writes a git config.
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=model",
            "-c",
            "user.email=model@example.com",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("git {args:?}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// A committed git workspace holding a repo.
pub struct Workspace {
    dir: TempDir,
    aster: PathBuf,
}

/// Set `RELATED_MODEL_KEEP=1` to leave each scratch repo on disk and print its
/// path, to rerun `aster` on it by hand.
const KEEP: &str = "RELATED_MODEL_KEEP";

impl Drop for Workspace {
    fn drop(&mut self) {
        if std::env::var_os(KEEP).is_some() {
            let kept =
                std::mem::replace(&mut self.dir, TempDir::new().expect("a scratch dir")).keep();
            #[allow(clippy::disallowed_macros)] // test-only diagnostic
            eprintln!("kept scratch repo {}", kept.display());
        }
    }
}

impl Workspace {
    pub fn commit(repo: &Repo) -> Result<Self, String> {
        let workspace = Workspace::empty()?;
        workspace.write(repo, &Changes::default())?;
        workspace.baseline()?;
        Ok(workspace)
    }

    /// A committed workspace holding exactly `files` (and an empty `aster.toml`).
    pub fn from_files(files: &[(&str, &str)]) -> Result<Self, String> {
        let workspace = Workspace::empty()?;
        workspace.put("aster.toml", "")?;
        for (path, content) in files {
            workspace.put(path, content)?;
        }
        workspace.baseline()?;
        Ok(workspace)
    }

    fn empty() -> Result<Self, String> {
        let workspace = Workspace {
            dir: TempDir::new().map_err(|e| e.to_string())?,
            aster: aster_binary()?,
        };
        git(workspace.root(), &["init", "-q", "-b", "main"])?;
        Ok(workspace)
    }

    fn baseline(&self) -> Result<(), String> {
        git(self.root(), &["add", "-A"])?;
        git(self.root(), &["commit", "-q", "-m", "baseline"])
    }

    fn put(&self, path: &str, content: &str) -> Result<(), String> {
        let full = self.root().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(full, content).map_err(|e| e.to_string())
    }

    /// Replace the first `from` with `to` in `path`, in the working tree.
    pub fn edit(&self, path: &str, from: &str, to: &str) -> Result<(), String> {
        let full = self.root().join(path);
        let content = fs::read_to_string(&full).map_err(|e| e.to_string())?;
        if !content.contains(from) {
            return Err(format!("{path} does not contain {from:?}"));
        }
        fs::write(full, content.replacen(from, to, 1)).map_err(|e| e.to_string())
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Write the repo with `changes` applied to the working tree.
    pub fn write(&self, repo: &Repo, changes: &Changes) -> Result<(), String> {
        for (path, content) in render::files(repo, changes) {
            self.put(&path, &content)?;
        }
        Ok(())
    }

    /// The dry-run plan of `aster affected test --related` against the baseline.
    pub fn plan(&self) -> Result<Plan, String> {
        let output = run(
            self.root(),
            &self.aster,
            &[
                "--json",
                "affected",
                "test",
                "--base=HEAD",
                "--related",
                "--dry-run",
            ],
        )?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            return Err(format!(
                "aster failed: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let json: Value =
            serde_json::from_str(&stdout).map_err(|e| format!("invalid JSON ({e}): {stdout}"))?;
        let address = format!("//{PROJECT}:test");
        let mut plan = Plan::default();
        for entry in json["targets"].as_array().into_iter().flatten() {
            if entry["address"].as_str() != Some(address.as_str()) {
                continue;
            }
            let strings = |key: &str| -> Vec<String> {
                entry[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            };
            plan.notes = strings("selection").join("\n");
            // A target with no replacement command runs as written: in full.
            let commands = strings("commands");
            plan.full |= commands.is_empty();
            for command in commands {
                let files: Vec<&str> = command
                    .split_whitespace()
                    .filter(|word| word.ends_with("_test.exs"))
                    .collect();
                if files.is_empty() {
                    plan.full = true;
                }
                plan.files.extend(files.into_iter().map(str::to_string));
            }
        }
        Ok(plan)
    }
}
