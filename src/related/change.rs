//! `aster analyze change`: the definitions and module edges of two
//! revisions of a source tree, for tools that measure a change.
//!
//! Each revision is read the way `--related` reads the workspace: every
//! source file is parsed by the same extractors (`scan`), and specifiers are
//! resolved by the same resolvers. Nothing is built or run. A file with a
//! syntax error is still read from the tree the parser recovered; the error
//! spans are listed, and the definitions they touch are marked `uncertain`.

use super::engine::{mark_rust_test_files, scan_files, MAX_SOURCE_BYTES};
use super::facts::{grammar, span, Def, DefKind, Family, FileFacts};
use super::resolve::{Resolvers, Workspace};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// The `schema` field of the output.
pub const SCHEMA: &str = "aster-change-v1";

/// Grammar crates the extractors are built with, as pinned in Cargo.toml.
/// A unit test keeps this list in step with the manifest.
pub const GRAMMARS: &[(&str, &str)] = &[
    ("tree-sitter", "0.26.13"),
    ("tree-sitter-elixir", "0.3.5"),
    ("tree-sitter-go", "0.25.0"),
    ("tree-sitter-python", "0.25.0"),
    ("tree-sitter-rust", "0.24.2"),
    ("tree-sitter-typescript", "0.23.2"),
];

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub tool: Tool,
    pub revisions: Sides<BTreeMap<String, FileReport>>,
    pub edges: Sides<Edges>,
    pub diff: Diff,
}

#[derive(Debug, Serialize)]
pub struct Tool {
    pub aster: &'static str,
    pub grammars: BTreeMap<&'static str, &'static str>,
}

#[derive(Debug, Serialize)]
pub struct Sides<T> {
    pub base: T,
    pub head: T,
}

/// One source file of a revision.
#[derive(Debug, Serialize)]
pub struct FileReport {
    pub family: Family,
    /// Test code: a test file, or a Rust file a `#[cfg(test)]` module holds.
    pub test_file: bool,
    pub parse_error: bool,
    /// 1-based inclusive line spans of the parser's error and missing nodes.
    pub error_lines: Vec<(usize, usize)>,
    /// Too large to parse, or not UTF-8: no definitions are known, and
    /// `top.hash` is the hash of the file's bytes.
    pub opaque: bool,
    /// Elixir modules defined here, or the Go package name.
    pub modules: Vec<String>,
    /// Loads code by a computed name, and how: the edges out of this file
    /// are incomplete.
    pub dynamic: Option<&'static str>,
    /// Calls `import()` or `require()` with a name that is not a literal:
    /// the edges out of this file are incomplete.
    pub open_loads: bool,
    pub imports: Vec<ImportReport>,
    pub reexports: Vec<String>,
    pub defs: Vec<DefReport>,
    /// Code outside every definition, as a nameless definition.
    pub top: DefReport,
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub lines: (usize, usize),
    /// Local names it binds.
    pub names: Vec<String>,
    pub specs: Vec<String>,
    /// The files the specifiers resolve to.
    pub targets: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DefReport {
    pub name: String,
    pub aliases: Vec<String>,
    pub kind: DefKind,
    pub lines: (usize, usize),
    /// The type a method belongs to.
    pub owner: Option<String>,
    /// SHA-256 (first 16 bytes, hex) of the tokens on the definition's
    /// lines, comments left out (in Python, with each line's indentation).
    /// Definitions that share a line share its tokens.
    pub hash: Option<String>,
    /// The Elixir module the definition is in.
    pub module: Option<String>,
    /// Test code, by its kind, its file, or a `#[cfg(test)]` module.
    pub test: bool,
    /// Invoked by a framework or runtime rather than by name.
    pub callback: bool,
    /// An error span overlaps it: the recovered tree may have its extent or
    /// its names wrong.
    pub uncertain: bool,
    pub refs: BTreeSet<String>,
    pub members: BTreeSet<String>,
    /// Specifiers of the files it draws names from.
    pub uses: BTreeSet<String>,
    /// Specifiers it loads by literal: `import()`, `require()`, a module mock.
    pub loaded: BTreeSet<String>,
    pub calls: Vec<CallReport>,
}

/// A call qualified by its module (Elixir, Go, Rust).
#[derive(Debug, Serialize)]
pub struct CallReport {
    pub module: String,
    pub function: String,
    /// Files the module resolves to; empty for code outside the tree.
    pub targets: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Edges {
    /// First-party edges between files (a Go package is its directory).
    pub resolved: Vec<Edge>,
    pub unresolved: Vec<Unresolved>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// `import`, `reexport`, `load` (a literal `import()`, `require()` or
    /// module mock), `call` (a module-qualified call), `module` (a Rust
    /// `mod name;`) or `path` (a JavaScript string literal that names the
    /// file, such as a script to spawn).
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Unresolved {
    pub from: String,
    pub spec: String,
    pub kind: &'static str,
    /// The specifier reads as naming workspace code (a relative path, a
    /// configured alias), so the tree is missing what it names.
    pub internal: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Diff {
    /// Files added, removed or modified, by path.
    pub files: BTreeMap<String, FileDiff>,
    pub edges_added: Vec<Edge>,
    pub edges_removed: Vec<Edge>,
}

#[derive(Debug, Default, Serialize)]
pub struct FileDiff {
    /// `added`, `removed` or `modified`.
    pub status: &'static str,
    /// Definitions by key: `owner.name` (for Elixir, `Module.name`), with
    /// `#n` for the n-th of the same key in the file.
    pub defs_added: Vec<String>,
    pub defs_removed: Vec<String>,
    pub defs_changed: Vec<String>,
    /// The code outside every definition changed.
    pub top_changed: bool,
}

/// Which files to report. Resolution always sees the whole tree.
#[derive(Debug, Default, Clone)]
pub struct Scope {
    /// Report only files under these directories (relative, `/`-separated);
    /// empty reports every file.
    pub paths: Vec<String>,
}

impl Scope {
    fn covers(&self, path: &str) -> bool {
        self.paths.is_empty()
            || self.paths.iter().any(|dir| {
                let dir = dir.trim_start_matches("./").trim_end_matches('/');
                dir.is_empty() || dir == "." || path == dir || path.starts_with(&format!("{dir}/"))
            })
    }
}

/// Compare the trees at `base` and `head`.
pub fn analyze_change(base: &Path, head: &Path, scope: &Scope) -> Report {
    let (base_files, base_edges) = describe(base, scope);
    let (head_files, head_edges) = describe(head, scope);
    let diff = diff(&base_files, &head_files, &base_edges, &head_edges);
    Report {
        schema: SCHEMA,
        tool: Tool {
            aster: env!("CARGO_PKG_VERSION"),
            grammars: GRAMMARS.iter().copied().collect(),
        },
        revisions: Sides {
            base: base_files,
            head: head_files,
        },
        edges: Sides {
            base: base_edges,
            head: head_edges,
        },
        diff,
    }
}

/// A relative path that stays inside the tree.
fn inside(path: &Path) -> bool {
    path.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Every source file of the tree at `root` in scope, and the edges out of
/// them. Every file under `root` but `.git` is read, whatever ignore files
/// say, so the result depends only on the files.
pub fn describe(root: &Path, scope: &Scope) -> (BTreeMap<String, FileReport>, Edges) {
    let (paths, mut facts, _) = scan_files(root, None, true);
    mark_rust_test_files(root, &mut facts);
    let ws = Workspace {
        root,
        paths: &paths,
        sources: &facts,
    };
    let resolvers = Resolvers::build(&ws);
    let mut ordered: Vec<&PathBuf> = facts
        .keys()
        .filter(|path| scope.covers(&display(path)))
        .collect();
    ordered.sort();
    let tokens = tokens_of(root, &ordered, &facts);

    let mut files = BTreeMap::new();
    let resolved = RefCell::new(BTreeSet::new());
    let unresolved = RefCell::new(BTreeSet::new());
    for (path, tokens) in ordered.iter().zip(tokens) {
        let file = &facts[*path];
        let from = display(path);
        let resolve = |spec: &str| -> Vec<String> {
            // Only files of the tree: a resolver may follow a path out of it
            // (a Rust `#[path]`), and what lies there is not the revision.
            let targets: BTreeSet<String> = resolvers
                .resolve(&ws, path, file.family, spec)
                .iter()
                .filter(|target| *target != *path && inside(target))
                .map(|target| display(target))
                .collect();
            targets.into_iter().collect()
        };
        let link = |spec: &str, kind: &'static str| -> Vec<String> {
            let targets = resolve(spec);
            // A Rust path that leads to no module of the tree resolves to its
            // own file: an item here, or a crate from elsewhere. Neither is
            // a missing file.
            if targets.is_empty() && file.family != Family::Rust {
                {
                    unresolved.borrow_mut().insert(Unresolved {
                        from: from.clone(),
                        spec: spec.to_string(),
                        kind,
                        internal: resolvers.internal(path, file.family, spec),
                    });
                }
            }
            for to in &targets {
                resolved.borrow_mut().insert(Edge {
                    from: from.clone(),
                    to: to.clone(),
                    kind,
                });
            }
            targets
        };

        let imports: Vec<ImportReport> = file
            .imports
            .iter()
            .map(|import| {
                let targets: BTreeSet<String> = import
                    .specs
                    .iter()
                    .flat_map(|spec| link(spec, "import"))
                    .collect();
                ImportReport {
                    lines: import.lines,
                    names: import.names.clone(),
                    specs: import.specs.clone(),
                    targets: targets.into_iter().collect(),
                }
            })
            .collect();
        for spec in &file.uses_all {
            link(spec, "import");
        }
        for spec in &file.reexports {
            link(spec, "reexport");
        }
        for (_, import) in &file.go_imports {
            link(import, "import");
        }
        // `mod name;` keeps a module in another file.
        for submodule in &file.submodules {
            let spec = ["self"]
                .into_iter()
                .chain(submodule.inline.iter().map(String::as_str))
                .chain([submodule.name.as_str()])
                .collect::<Vec<_>>()
                .join("::");
            let found: Vec<String> = resolvers
                .rust
                .follow(path, &spec)
                .into_iter()
                .filter(|(target, rest)| rest.is_empty() && target != *path && inside(target))
                .map(|(target, _)| display(&target))
                .collect();
            if found.is_empty() {
                unresolved.borrow_mut().insert(Unresolved {
                    from: from.clone(),
                    spec,
                    kind: "module",
                    internal: true,
                });
            }
            for to in found {
                resolved.borrow_mut().insert(Edge {
                    from: from.clone(),
                    to,
                    kind: "module",
                });
            }
        }
        let imported: BTreeSet<&str> = file
            .imports
            .iter()
            .flat_map(|import| import.specs.iter().map(String::as_str))
            .collect();
        // The Elixir module each definition is in: the innermost module body
        // around it, or the file's one module.
        let module_of = |def: &Def| -> Option<String> {
            if file.family != Family::Elixir {
                return None;
            }
            file.defs
                .iter()
                .filter(|m| m.body && m.lines.0 <= def.lines.0 && def.lines.1 <= m.lines.1)
                .min_by_key(|m| m.lines.1 - m.lines.0)
                .map(|m| m.name.clone())
                .or_else(|| (file.modules.len() == 1).then(|| file.modules[0].clone()))
        };
        let bound: BTreeSet<&str> = imported
            .iter()
            .copied()
            .chain(file.uses_all.iter().map(String::as_str))
            .chain(file.reexports.iter().map(String::as_str))
            .collect();
        let report_def = |def: &Def, hash: Option<String>, uncertain: bool| -> DefReport {
            for spec in &def.uses {
                // A JavaScript definition uses a file through an import, a
                // literal `import()` or `require()` (a load, below), or a
                // relative string literal that names it (a path: data, a
                // file to spawn, not an import).
                let kind = match file.family {
                    Family::Js if bound.contains(spec.as_str()) => "import",
                    Family::Js if def.loaded.contains(spec) => continue,
                    Family::Js => "path",
                    _ => "import",
                };
                link(spec, kind);
            }
            // The top's `loaded` also holds the static imports.
            for spec in def.loaded.iter().filter(|s| !imported.contains(s.as_str())) {
                link(spec, "load");
            }
            let calls = def
                .calls
                .iter()
                .map(|(module, function)| {
                    let targets = match file.family {
                        Family::Go => file
                            .go_imports
                            .iter()
                            .filter(|(alias, import)| {
                                resolvers.go_qualifier(import, alias.as_deref()) == *module
                            })
                            .flat_map(|(_, import)| resolve(import))
                            .collect(),
                        _ => resolve(module),
                    };
                    for to in &targets {
                        resolved.borrow_mut().insert(Edge {
                            from: from.clone(),
                            to: to.clone(),
                            kind: "call",
                        });
                    }
                    CallReport {
                        module: module.clone(),
                        function: function.clone(),
                        targets,
                    }
                })
                .collect();
            DefReport {
                name: def.name.clone(),
                aliases: def.aliases.clone(),
                kind: def.kind,
                lines: def.lines,
                owner: def.owner.clone(),
                hash,
                module: if def.body { None } else { module_of(def) },
                test: def.is_test() || def.in_test || file.is_test_file,
                callback: def.callback,
                uncertain,
                refs: def.refs.clone(),
                members: def.members.clone(),
                uses: def.uses.clone(),
                loaded: def.loaded.clone(),
                calls,
            }
        };
        let overlaps = |lines: (usize, usize)| {
            tokens
                .errors
                .iter()
                .any(|&(start, end)| start <= lines.1 && lines.0 <= end)
        };
        let defs: Vec<DefReport> = file
            .defs
            .iter()
            .map(|def| {
                let hash = tokens.hash(|line| def.lines.0 <= line && line <= def.lines.1);
                report_def(def, hash, overlaps(def.lines))
            })
            .collect();
        let top_hash = tokens.raw.clone().or_else(|| {
            tokens.hash(|line| {
                matches!(
                    file.class_of(line),
                    super::facts::LineClass::Top | super::facts::LineClass::Import(_)
                )
            })
        });
        let top_uncertain = tokens.errors.iter().any(|&(start, end)| {
            (start..=end).any(|line| {
                matches!(
                    file.class_of(line),
                    super::facts::LineClass::Top | super::facts::LineClass::Import(_)
                )
            })
        });
        let top = report_def(&file.top, top_hash, top_uncertain);
        files.insert(
            from.clone(),
            FileReport {
                family: file.family,
                test_file: file.is_test_file,
                parse_error: file.parse_error,
                error_lines: tokens.errors.clone(),
                opaque: tokens.raw.is_some(),
                modules: file.modules.clone(),
                dynamic: file.dynamic,
                open_loads: file.open_loads,
                imports,
                reexports: file.reexports.clone(),
                defs,
                top,
            },
        );
    }
    let edges = Edges {
        resolved: resolved.into_inner().into_iter().collect(),
        unresolved: unresolved.into_inner().into_iter().collect(),
    };
    (files, edges)
}

/// A file's code tokens by line, and its error spans.
#[derive(Default)]
struct Tokens {
    /// `(1-based line, text)` of every leaf outside comments, in order.
    leaves: Vec<(usize, String)>,
    errors: Vec<(usize, usize)>,
    /// For a file that could not be parsed (too large, not UTF-8), the hash
    /// of its bytes.
    raw: Option<String>,
}

fn digest(bytes: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut hasher = Sha256::new();
    for part in bytes {
        hasher.update(part.as_ref());
    }
    hasher.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Tokens {
    fn hash(&self, keep: impl Fn(usize) -> bool) -> Option<String> {
        if self.raw.is_some() {
            return None;
        }
        Some(digest(
            self.leaves
                .iter()
                .filter(|(line, _)| keep(*line))
                .flat_map(|(_, text)| [text.as_bytes(), &[0x1f]]),
        ))
    }
}

/// Parse each file once more for its tokens and error spans, in parallel.
/// The facts say which files the extractors could read; the tokens are
/// what a definition's hash is made of.
fn tokens_of(root: &Path, paths: &[&PathBuf], facts: &HashMap<PathBuf, FileFacts>) -> Vec<Tokens> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(paths.len().max(1));
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|path| file_tokens(root, path, facts[*path].family))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        // A lost chunk would pair later files with the wrong tokens.
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

fn file_tokens(root: &Path, path: &Path, family: Family) -> Tokens {
    let bytes = std::fs::read(root.join(path)).unwrap_or_default();
    let opaque = |bytes: &[u8]| Tokens {
        raw: Some(digest([bytes])),
        ..Tokens::default()
    };
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return opaque(&bytes);
    }
    let Ok(source) = std::str::from_utf8(&bytes) else {
        return opaque(&bytes);
    };
    let mut parser = tree_sitter::Parser::new();
    let tree = parser
        .set_language(&grammar(family, path))
        .ok()
        .and_then(|_| parser.parse(source, None));
    let Some(tree) = tree else {
        return opaque(&bytes);
    };
    // Python's indentation is syntax; its tokens have no width, so the
    // column of a line's first token stands for them.
    let mut last_row = usize::MAX;
    let mut tokens = Tokens::default();
    let mut spans = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            spans.push(span(node));
        }
        if node.kind().contains("comment") {
            continue;
        }
        if node.child_count() == 0 {
            let text = &source[node.byte_range()];
            if !text.is_empty() {
                let start = node.start_position();
                if family == Family::Python && start.row != last_row {
                    tokens
                        .leaves
                        .push((start.row + 1, format!("\u{1}{}", start.column)));
                }
                last_row = start.row;
                tokens.leaves.push((start.row + 1, text.to_string()));
            }
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    spans.sort();
    for (start, end) in spans {
        match tokens.errors.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => tokens.errors.push((start, end)),
        }
    }
    tokens
}

/// Keys for a file's definitions: `owner.name`, numbered when repeated.
fn keyed(defs: &[DefReport]) -> BTreeMap<String, Option<&String>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    defs.iter()
        .map(|def| {
            let key = match def.owner.as_ref().or(def.module.as_ref()) {
                Some(owner) => format!("{owner}.{}", def.name),
                None => def.name.clone(),
            };
            let n = seen.entry(key.clone()).or_default();
            *n += 1;
            let key = if *n > 1 { format!("{key}#{n}") } else { key };
            (key, def.hash.as_ref())
        })
        .collect()
}

fn diff(
    base: &BTreeMap<String, FileReport>,
    head: &BTreeMap<String, FileReport>,
    base_edges: &Edges,
    head_edges: &Edges,
) -> Diff {
    let mut files = BTreeMap::new();
    let paths: BTreeSet<&String> = base.keys().chain(head.keys()).collect();
    for path in paths {
        let empty = Vec::new();
        let (old, new) = (base.get(path), head.get(path));
        let old_defs = keyed(old.map_or(&empty, |f| &f.defs));
        let new_defs = keyed(new.map_or(&empty, |f| &f.defs));
        let file = FileDiff {
            status: match (old, new) {
                (None, _) => "added",
                (_, None) => "removed",
                _ => "modified",
            },
            defs_added: new_defs
                .keys()
                .filter(|k| !old_defs.contains_key(*k))
                .cloned()
                .collect(),
            defs_removed: old_defs
                .keys()
                .filter(|k| !new_defs.contains_key(*k))
                .cloned()
                .collect(),
            defs_changed: new_defs
                .iter()
                .filter(|(k, hash)| old_defs.get(*k).is_some_and(|old| old != *hash))
                .map(|(k, _)| k.clone())
                .collect(),
            top_changed: match (old, new) {
                (Some(old), Some(new)) => old.top.hash != new.top.hash,
                _ => false,
            },
        };
        let unchanged = file.status == "modified"
            && file.defs_added.is_empty()
            && file.defs_removed.is_empty()
            && file.defs_changed.is_empty()
            && !file.top_changed;
        if !unchanged {
            files.insert(path.clone(), file);
        }
    }
    let old: BTreeSet<&Edge> = base_edges.resolved.iter().collect();
    let new: BTreeSet<&Edge> = head_edges.resolved.iter().collect();
    Diff {
        files,
        edges_added: new.difference(&old).map(|e| (*e).clone()).collect(),
        edges_removed: old.difference(&new).map(|e| (*e).clone()).collect(),
    }
}

/// The facts `extract` reads from one file; for tests.
#[cfg(test)]
fn facts_of(path: &str, source: &str) -> FileFacts {
    let path = Path::new(path);
    super::engine::extract(Family::of(path).expect("a source file"), path, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_versions_match_the_manifest() {
        let manifest = include_str!("../../Cargo.toml");
        for (name, version) in GRAMMARS {
            let line = format!("{name} = \"{version}\"");
            assert!(manifest.contains(&line), "Cargo.toml has no `{line}`");
        }
    }

    #[test]
    fn a_definition_hash_ignores_comments_and_layout_but_not_code() {
        let dir = tempfile::tempdir().unwrap();
        let write = |text: &str| {
            std::fs::write(dir.path().join("a.ts"), text).unwrap();
            file_tokens(dir.path(), Path::new("a.ts"), Family::Js)
        };
        let plain = write("export function f(x: number) {\n  return x + 1;\n}\n");
        let commented =
            write("// adds one\nexport function f(x: number) {\n  return x + 1; // one\n}\n");
        let changed = write("export function f(x: number) {\n  return x + 2;\n}\n");
        let all = |_: usize| true;
        assert_eq!(plain.hash(all), commented.hash(all));
        assert_ne!(plain.hash(all), changed.hash(all));
        assert!(plain.errors.is_empty());
    }

    #[test]
    fn error_spans_are_merged_and_the_file_keeps_its_definitions() {
        let source = "export function ok() {\n  return 1;\n}\nexport function bad( {\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.ts"), source).unwrap();
        let tokens = file_tokens(dir.path(), Path::new("b.ts"), Family::Js);
        assert!(!tokens.errors.is_empty());
        assert!(tokens.errors.iter().all(|(start, _)| *start >= 4));
        let facts = facts_of("b.ts", source);
        assert!(facts.parse_error);
        assert!(facts.defs.iter().any(|d| d.name == "ok"));
    }

    #[test]
    fn repeated_definitions_get_numbered_keys() {
        let def = |name: &str, owner: Option<&str>| DefReport {
            name: name.into(),
            aliases: vec![],
            kind: DefKind::Function,
            lines: (1, 1),
            owner: owner.map(str::to_string),
            hash: None,
            module: None,
            test: false,
            callback: false,
            uncertain: false,
            refs: BTreeSet::new(),
            members: BTreeSet::new(),
            uses: BTreeSet::new(),
            loaded: BTreeSet::new(),
            calls: vec![],
        };
        let defs = [def("run", None), def("run", None), def("run", Some("Job"))];
        let keys: Vec<String> = keyed(&defs).into_keys().collect();
        assert_eq!(keys, ["Job.run", "run", "run#2"]);
    }

    #[test]
    fn scope_matches_whole_directory_names() {
        let scope = Scope {
            paths: vec!["src/app/".into()],
        };
        assert!(scope.covers("src/app/a.ts"));
        assert!(!scope.covers("src/application/a.ts"));
        assert!(Scope::default().covers("anything.rs"));
    }
}
