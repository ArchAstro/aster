//! The source-level rules of `aster check`: what the facts and the call
//! sites around string literals say about a workspace's annotations and
//! `[consumes]` entries.

use super::annotations::{self, Directive};
use super::consumers;
use super::engine::{owner, scan};
use super::facts::{Family, FileFacts};
use super::resolve::RustIndex;
use super::sites::{self, Role};
use crate::config::{find_aster_toml, parse_aster_toml};
use crate::discovery::DiscoveredProject;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The facts of every source file of a workspace and the files it holds.
pub struct Sources {
    root: PathBuf,
    pub facts: HashMap<PathBuf, FileFacts>,
    /// Every file, workspace-relative.
    files: HashSet<PathBuf>,
    /// Every directory that holds a file.
    dirs: HashSet<PathBuf>,
    /// Source files whose facts the cache held.
    pub reused: usize,
}

impl Sources {
    /// Read the workspace's facts through the cache in `cache_dir`.
    pub fn load(root: &Path, cache_dir: Option<&Path>) -> Self {
        let (files, mut facts, reused) = scan(root, cache_dir.map(super::cache::FactsCache::open));
        // Which Rust files are test code is decided by the module that
        // declares them, not by the file.
        let rust = RustIndex::build(
            root,
            facts
                .iter()
                .filter(|(_, f)| f.family == Family::Rust)
                .map(|(path, _)| path),
        );
        for file in rust.test_files(&facts) {
            if let Some(facts) = facts.get_mut(&file) {
                facts.is_test_file = true;
            }
        }
        let dirs = files
            .iter()
            .flat_map(|f| f.ancestors().skip(1))
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        Self {
            root: root.to_path_buf(),
            facts,
            files,
            dirs,
            reused,
        }
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.contains(path) || self.dirs.contains(path)
    }

    /// Whether anything in the workspace matches a `reads` pattern.
    fn matches_any(&self, pattern: &str) -> bool {
        if !pattern.contains(['*', '?', '[', '{']) {
            return self.exists(Path::new(pattern));
        }
        self.files.iter().any(|f| annotations::covers(pattern, f))
    }
}

/// A finding, before it is given a rule id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: PathBuf,
    pub line: Option<usize>,
    /// The project address, for a finding about its configuration.
    pub project: Option<String>,
    pub message: String,
}

/// Whether the text of a join's base names the workspace root (or a
/// directory resolved from the code's own location), where a joined
/// literal is plausibly a read of the repository.
fn names_workspace_root(base: &str) -> bool {
    let lower = base.to_lowercase();
    let scratch = ["tmp", "temp", "sandbox", "scratch", "fixture"];
    if scratch.iter().any(|word| lower.contains(word)) {
        return false;
    }
    [
        "cwd",
        "getcwd",
        "root",
        "workspace",
        "repo",
        "__dirname",
        "__dir__",
        "__file__",
        "dirname",
        "manifest_dir",
        "pwd",
        "project_dir",
        "project_path",
        "base_dir",
        "basedir",
        "base_path",
        "git_top",
        "toplevel",
        "import.meta",
        "here",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// The right-hand side of the first assignment to `name` in `source`, for a
/// base that is a variable (`@root`, `self.work`, `checkout`).
fn assigned_to(name: &str, source: &str) -> Option<String> {
    let attribute = name.trim().starts_with('@');
    let name = name
        .trim()
        .trim_start_matches('@')
        .trim_start_matches("self.")
        .trim_start_matches("this.");
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
    {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        let mut rest = line.trim_start();
        for keyword in [
            "const ", "let ", "var ", "val ", "final ", "mut ", "export ",
        ] {
            rest = rest.strip_prefix(keyword).unwrap_or(rest);
        }
        let rest = rest
            .trim_start_matches('@')
            .trim_start_matches("self.")
            .trim_start_matches("this.");
        let Some(after) = rest.strip_prefix(name) else {
            continue;
        };
        // An Elixir module attribute is set without an operator.
        let spaced = attribute && after.starts_with(char::is_whitespace);
        let after = after.trim_start();
        let value = after
            .strip_prefix(":=")
            .or_else(|| after.strip_prefix('=').filter(|r| !r.starts_with('=')))
            .or_else(|| spaced.then_some(after));
        if let Some(value) = value {
            let value = value.trim();
            return Some(if value.is_empty() {
                lines
                    .get(at + 1)
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default()
            } else {
                value.to_string()
            });
        }
    }
    None
}

/// Whether a join's base plausibly is the repository: it names the root, or
/// is a variable assigned from something that does, or a path literal
/// (resolved from the working directory).
fn base_is_workspace(base: &str, source: &str) -> bool {
    let base = base.trim();
    base.starts_with(['"', '\'', '`'])
        || names_workspace_root(base)
        || assigned_to(base, source).is_some_and(|value| names_workspace_root(&value))
}

/// Literals in tests that name an existing workspace path as the target of
/// a write, or as a later operand of a join whose base is not the workspace
/// root, and that no `aster:` comment covers.
pub fn suspicious_path_literals(projects: &[DiscoveredProject], src: &Sources) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut files: Vec<_> = src.facts.iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (path, facts) in files {
        let project_dir = owner(projects, path)
            .map(|p| projects[p].relative_path.clone())
            .unwrap_or_default();
        let file_dir = path.parent().unwrap_or(Path::new(""));
        // (line, literal) -> the workspace path it names.
        let mut wanted: HashMap<(usize, String), PathBuf> = HashMap::new();
        for (index, def) in facts.defs.iter().chain([&facts.top]).enumerate() {
            if !(facts.is_test_file || def.in_test) {
                continue;
            }
            // `top` follows the definitions; annotations outside every
            // definition have no owner.
            let owner = (index < facts.defs.len()).then_some(index as u32);
            let declared: Vec<&String> = facts
                .annotations
                .iter()
                .filter(|a| a.owner == owner)
                .filter_map(|a| match &a.directive {
                    Directive::Reads(paths) => Some(paths),
                    _ => None,
                })
                .flatten()
                .collect();
            for (text, line) in def.strings.iter().zip(&def.string_lines) {
                if def.loaded.contains(text) {
                    continue;
                }
                let existing = consumers::named_paths(text, file_dir, &project_dir)
                    .into_iter()
                    .find(|candidate| src.exists(candidate));
                if let Some(existing) = existing {
                    if declared.iter().any(|p| declares_read(p, &existing)) {
                        continue;
                    }
                    wanted.insert((*line as usize, text.clone()), existing);
                }
            }
        }
        if wanted.is_empty() {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(src.root.join(path)) else {
            continue;
        };
        let keys: HashSet<(usize, String)> = wanted.keys().cloned().collect();
        for site in sites::sites(facts.family, path, &source, &keys) {
            let named = &wanted[&(site.line, site.text.clone())];
            let how = match &site.role {
                Role::Write { callee } => format!("is an argument of `{callee}`, which writes"),
                Role::Join { callee, base } => {
                    // A `../x` joined onto a base is relative to that base,
                    // usually the file's own directory.
                    if base_is_workspace(base, &source)
                        || site.text.starts_with('.') && site.text.contains("..")
                    {
                        continue;
                    }
                    format!(
                        "is joined onto `{}` by `{callee}`, which is not the workspace root",
                        shorten(base)
                    )
                }
            };
            findings.push(Finding {
                file: path.clone(),
                line: Some(site.line),
                project: None,
                message: format!(
                    "\"{}\" names {} and {how}. If the test does not read the repository's \
                     file, add `aster: not-a-read` on this line or the line before; if it does, \
                     leave it or declare more with `aster: reads`",
                    shorten(&site.text),
                    named.display()
                ),
            });
        }
    }
    findings
}

/// Whether an `aster: reads` entry declares the read that a literal naming
/// `named` stands for: the entry matches `named` or lies beneath it, or
/// `named` is the trailing whole components of the entry's directory (the
/// literal is joined onto a base other than the root), or a glob tail of
/// the entry matches it.
fn declares_read(pattern: &str, named: &Path) -> bool {
    if annotations::covers(pattern, named) {
        return true;
    }
    let prefix = annotations::literal_prefix(pattern);
    if prefix.starts_with(named) {
        return true;
    }
    let want: Vec<_> = named.components().collect();
    let have: Vec<_> = prefix.components().collect();
    if !want.is_empty() && have.ends_with(&want) {
        return true;
    }
    let parts: Vec<&str> = pattern.split('/').collect();
    (1..parts.len()).any(|at| {
        !parts[at].contains(['*', '?', '[', '{'])
            && annotations::covers(&parts[at..].join("/"), named)
    })
}

fn shorten(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 48 {
        format!("{}...", flat.chars().take(45).collect::<String>())
    } else {
        flat
    }
}

/// Annotations that do nothing or that Aster does not understand.
pub fn stale_annotations(projects: &[DiscoveredProject], src: &Sources) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut files: Vec<_> = src
        .facts
        .iter()
        .filter(|(_, f)| !f.annotations.is_empty())
        .collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (path, facts) in files {
        let project_dir = owner(projects, path)
            .map(|p| projects[p].relative_path.clone())
            .unwrap_or_default();
        let file_dir = path.parent().unwrap_or(Path::new(""));
        for annotation in &facts.annotations {
            let message = match &annotation.directive {
                Directive::NotARead => {
                    let names = annotation
                        .suppressed
                        .iter()
                        .any(|s| !consumers::named_paths(s, file_dir, &project_dir).is_empty());
                    (!names).then(|| {
                        format!(
                            "`aster: not-a-read` covers line {}, which holds no string literal \
                             that names a workspace path; remove it or move it to the line it \
                             means",
                            annotation.target
                        )
                    })
                }
                Directive::Reads(paths) if paths.is_empty() => {
                    Some("`aster: reads` names no path".to_string())
                }
                Directive::Reads(paths) => {
                    let missing: Vec<&String> = paths
                        .iter()
                        .filter(|pattern| !src.matches_any(pattern))
                        .collect();
                    (!missing.is_empty()).then(|| {
                        format!(
                            "`aster: reads` names {} that match no file or directory of the \
                             workspace (paths are workspace-relative)",
                            missing
                                .iter()
                                .map(|p| format!("`{p}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                }
                Directive::Unknown(name) => Some(format!(
                    "unknown directive `aster: {name}`; the known ones are `not-a-read` and \
                     `reads <path>...`"
                )),
            };
            if let Some(message) = message {
                findings.push(Finding {
                    file: path.clone(),
                    line: Some(annotation.line),
                    project: None,
                    message,
                });
            }
        }
    }
    findings
}

/// `[consumes]` entries that name something that is not a project, or list
/// a `files` pattern that matches no source file.
pub fn bad_consumes(projects: &[DiscoveredProject], src: &Sources) -> Vec<Finding> {
    let addresses: HashSet<String> = projects
        .iter()
        .map(|p| format!("//{}", p.relative_path.display()))
        .collect();
    let mut findings = Vec::new();
    for project in projects {
        let Some(config_path) = find_aster_toml(&project.root) else {
            continue;
        };
        let Ok(config) = parse_aster_toml(&config_path) else {
            continue;
        };
        let file = config_path
            .strip_prefix(&src.root)
            .unwrap_or(&config_path)
            .to_path_buf();
        let address = format!("//{}", project.relative_path.display());
        for (key, entry) in &config.consumes {
            let mut report = |message: String| {
                findings.push(Finding {
                    file: file.clone(),
                    line: None,
                    project: Some(address.clone()),
                    message,
                });
            };
            if !addresses.contains(key) {
                report(format!(
                    "[consumes.\"{key}\"] is not a project of this workspace"
                ));
                continue;
            }
            for pattern in entry.files.iter().flatten() {
                let (base, glob) = match pattern.strip_prefix("//") {
                    Some(rest) => (PathBuf::new(), rest),
                    None => (project.relative_path.clone(), pattern.as_str()),
                };
                let Ok(glob) = globset::Glob::new(glob) else {
                    continue;
                };
                let matcher = glob.compile_matcher();
                let matched = src.facts.keys().any(|source| {
                    source
                        .strip_prefix(&base)
                        .is_ok_and(|relative| matcher.is_match(relative))
                });
                if !matched {
                    report(format!(
                        "[consumes.\"{key}\"] lists `{pattern}`, which matches no source file in \
                         Elixir, TypeScript/JavaScript, Go, Python or Rust; Aster ignores the \
                         entry and runs the project in full"
                    ));
                }
            }
        }
    }
    findings
}
