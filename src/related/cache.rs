//! Persistent per-file facts for `--related`.
//!
//! Parsing every source file with tree-sitter dominates a run on a large
//! workspace and gives the same facts until the file changes. Facts are
//! stored one file per source file under
//! `<cache dir>/related-facts/<build>/<xx>/<key>.json`:
//!
//! - `build` hashes the Aster version, `Cargo.lock` (the grammar versions)
//!   and the sources of the extractors, so any change to how facts are read
//!   starts a fresh directory.
//! - `key` hashes the file's workspace-relative path and its content.
//! - Entries are written to a temporary name and renamed into place, so
//!   concurrent Aster processes see a whole entry or none, and the same key
//!   always holds the same facts.
//! - At the end of a run, entries no file of the workspace still maps to and
//!   directories of other builds are deleted, which bounds the cache by the
//!   size of the workspace.
//!
//! A cache that cannot be read or written is ignored: the file is parsed.

use super::facts::FileFacts;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const ROOT: &str = "related-facts";

/// Bump when the stored shape of facts changes without any extractor source
/// changing. Facts that carry `aster:` annotations are format 2.
const FORMAT: &str = "facts-format-2";

/// Everything that decides what facts a given source text yields.
fn build_id() -> String {
    let mut hash = Sha256::new();
    for part in [
        FORMAT,
        env!("CARGO_PKG_VERSION"),
        include_str!("annotations.rs"),
        include_str!("../../Cargo.lock"),
        include_str!("facts.rs"),
        include_str!("elixir.rs"),
        include_str!("go.rs"),
        include_str!("js.rs"),
        include_str!("python.rs"),
        include_str!("rust.rs"),
    ] {
        hash.update(part.len().to_le_bytes());
        hash.update(part.as_bytes());
    }
    hex(&hash.finalize()[..8])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct FactsCache {
    /// `<cache dir>/related-facts`.
    root: PathBuf,
    /// `root/<build>`.
    dir: PathBuf,
    build: String,
}

/// The cache key of one file.
pub type Key = String;

impl FactsCache {
    pub fn open(cache_dir: &Path) -> Self {
        let root = cache_dir.join(ROOT);
        let build = build_id();
        Self {
            dir: root.join(&build),
            root,
            build,
        }
    }

    pub fn key(path: &Path, source: &str) -> Key {
        let mut hash = Sha256::new();
        let path = path.to_string_lossy();
        hash.update(path.len().to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(source.as_bytes());
        hex(&hash.finalize()[..16])
    }

    fn entry(&self, key: &str) -> PathBuf {
        self.dir.join(&key[..2]).join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<FileFacts> {
        let bytes = fs::read(self.entry(key)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn put(&self, key: &str, facts: &FileFacts) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let target = self.entry(key);
        let Some(parent) = target.parent() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(facts) else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary = parent.join(format!(
            ".{key}.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::write(&temporary, bytes).is_err() || fs::rename(&temporary, &target).is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }

    /// Delete entries for keys outside `used`, abandoned temporary files and
    /// the directories of other builds.
    pub fn prune(&self, used: &HashSet<Key>) {
        let stale_after = std::time::Duration::from_secs(3600);
        if let Ok(builds) = fs::read_dir(&self.root) {
            for build in builds.flatten() {
                if build.file_name().to_string_lossy() != self.build {
                    let _ = fs::remove_dir_all(build.path());
                }
            }
        }
        let Ok(shards) = fs::read_dir(&self.dir) else {
            return;
        };
        for shard in shards.flatten() {
            let Ok(entries) = fs::read_dir(shard.path()) else {
                continue;
            };
            let mut left = 0usize;
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let remove = match name.strip_suffix(".json") {
                    Some(key) => !used.contains(key),
                    // A temporary file may belong to a run in progress.
                    None => entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > stale_after),
                };
                if remove {
                    let _ = fs::remove_file(entry.path());
                } else {
                    left += 1;
                }
            }
            if left == 0 {
                let _ = fs::remove_dir(shard.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::related::{elixir, go, js, python, rust};

    #[test]
    fn stored_facts_read_back_identical_for_every_language() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = FactsCache::open(dir.path());
        let samples: Vec<(&str, FileFacts)> = vec![
            (
                "a.ex",
                elixir::extract(Path::new("a.ex"), "defmodule A do\n  def f, do: Enum.map([], &g/1)\nend\n"),
            ),
            (
                "a.ts",
                js::extract(Path::new("a.ts"), "import { x } from \"./b\";\nexport const f = () => x;\n"),
            ),
            ("a.go", go::extract(Path::new("a.go"), "package a\nfunc F() {}\n")),
            ("a.py", python::extract(Path::new("a.py"), "def f():\n    pass\n")),
            (
                "a.rs",
                rust::extract(
                    Path::new("a.rs"),
                    "#[cfg(test)]\nmod t { #[test] fn x() { std::process::Command::new(env!(\"CARGO_BIN_EXE_a\")); } }\n",
                ),
            ),
        ];
        for (name, facts) in samples {
            let key = FactsCache::key(Path::new(name), "source");
            assert!(cache.get(&key).is_none());
            cache.put(&key, &facts);
            let read = cache.get(&key).unwrap();
            assert_eq!(format!("{facts:?}"), format!("{read:?}"), "{name}");
        }
    }

    #[test]
    fn a_key_depends_on_path_and_content() {
        let a = FactsCache::key(Path::new("a.ts"), "x");
        assert_eq!(a, FactsCache::key(Path::new("a.ts"), "x"));
        assert_ne!(a, FactsCache::key(Path::new("b.ts"), "x"));
        assert_ne!(a, FactsCache::key(Path::new("a.ts"), "y"));
    }
}
