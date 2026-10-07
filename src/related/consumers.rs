//! How a project consumes a dependency the source graph cannot follow: a
//! built artifact, a binary its tests launch, files they read from the
//! dependency's directory.
//!
//! Evidence comes from two places and is never guessed:
//!
//! - `[consumes."//dependency"]` in the project's `aster.toml`: `files`
//!   names the sources that use the dependency, and `infer = true` also
//!   counts every definition that names a path inside the dependency.
//! - Without an entry, nothing narrows the project. The analysis only
//!   reports how many definitions name the dependency, as a hint.
//!
//! A definition names a path when one of its string literals contains it:
//! workspace-relative (`services/api/configs/x.yaml`, `//services/api`), or
//! relative to the file or to the project directory (`../../api/configs`,
//! the two bases a test resolves `__DIR__` and the working directory
//! against). Anything built piecewise, read from the environment or kept in
//! a file that is not source is invisible, which is why `infer` is opt-in.

use crate::config::{find_aster_toml, parse_aster_toml};
use crate::discovery::DiscoveredProject;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// One `[consumes]` entry.
#[derive(Debug, Clone)]
pub struct Consume {
    pub infer: bool,
    /// Glob patterns of the sources that consume the dependency, when the
    /// project states them.
    pub files: Option<Vec<String>>,
}

/// The entries of each project, keyed by project index and then by the
/// index of the dependency.
pub fn load(projects: &[DiscoveredProject]) -> Vec<HashMap<usize, Consume>> {
    let index: HashMap<String, usize> = projects
        .iter()
        .enumerate()
        .map(|(i, p)| (format!("//{}", p.relative_path.display()), i))
        .collect();
    projects
        .iter()
        .map(|project| {
            let mut entries = HashMap::new();
            let Some(path) = find_aster_toml(&project.root) else {
                return entries;
            };
            let Ok(config) = parse_aster_toml(&path) else {
                return entries;
            };
            for (key, entry) in config.consumes {
                if let Some(&dependency) = index.get(&key) {
                    entries.insert(
                        dependency,
                        Consume {
                            infer: entry.infer.unwrap_or(false),
                            files: entry.files,
                        },
                    );
                }
            }
            entries
        })
        .collect()
}

/// Lexically resolve `path` against `base`; `None` when it leaves the
/// workspace.
fn resolve(base: &Path, path: &str) -> Option<PathBuf> {
    let mut parts: Vec<std::ffi::OsString> = base
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name.into()),
        }
    }
    Some(parts.into_iter().collect())
}

/// The workspace paths a string literal can name from a file in
/// `file_dir` of the project at `project_dir`.
pub fn named_paths(text: &str, file_dir: &Path, project_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for token in text.split(|c: char| {
        !(c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '@' | '+' | '-' | ':'))
    }) {
        // An address names the project; a bare word names nothing.
        let token = token.split(':').next().unwrap_or(token);
        let token = token.trim_end_matches('.');
        if !token.contains('/') {
            continue;
        }
        let relative = token.starts_with("./") || token.starts_with("../");
        let candidates: Vec<PathBuf> = if relative {
            [file_dir, project_dir]
                .into_iter()
                .filter_map(|base| resolve(base, token))
                .collect()
        } else if let Some(address) = token.strip_prefix("//") {
            resolve(Path::new(""), address).into_iter().collect()
        } else if token.starts_with('/') {
            // An absolute path or a URL path.
            Vec::new()
        } else {
            resolve(Path::new(""), token).into_iter().collect()
        };
        for candidate in candidates {
            if candidate
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
                && !candidate.as_os_str().is_empty()
                && !found.contains(&candidate)
            {
                found.push(candidate);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_paths_from_each_base() {
        let file_dir = Path::new("services/platform/test/archdev");
        let project = Path::new("services/platform");
        let named = |text: &str| named_paths(text, file_dir, project);
        assert!(named("../../../go/archdev/configs")
            .contains(&PathBuf::from("services/go/archdev/configs")));
        assert!(named("../../src/ts/archdev").contains(&PathBuf::from("src/ts/archdev")));
        assert_eq!(
            named("services/go/archdev/x.yaml"),
            [PathBuf::from("services/go/archdev/x.yaml")]
        );
        assert_eq!(
            named("//services/go/archdev:build"),
            [PathBuf::from("services/go/archdev")]
        );
        assert!(named("/api/users").is_empty());
        assert!(named("archdev").is_empty());
        // Escaping the workspace names nothing.
        assert!(named("../../../../../../../etc/passwd").is_empty());
    }
}
