//! The workspace source graph and the walk from a change to the tests that
//! can observe it.

use super::consumers::{self, Consume};
use super::facts::{DefKind, Family, FileFacts, LineClass};
use super::resolve::{Resolvers, Workspace};
use super::{elixir, go, js, python};
use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Directories never worth reading, tracked or not.
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "_build",
    "deps",
    "target",
    "vendor",
    ".git",
    ".aster",
];
/// Larger files are generated or bundled; they are treated as opaque.
const MAX_SOURCE_BYTES: u64 = 1_500_000;
/// Reasons listed per test before the rest are counted.
const MAX_REASONS: usize = 1;

/// One changed file, relative to the workspace root.
#[derive(Debug, Clone, Default)]
pub struct Change {
    pub path: PathBuf,
    pub deleted: bool,
    /// Changed line spans (1-based, inclusive) in the new file; `None` when
    /// unknown, which counts as the whole file.
    pub new_lines: Option<Vec<(usize, usize)>>,
    /// Changed line spans in the old file.
    pub old_lines: Option<Vec<(usize, usize)>>,
    pub old_source: Option<String>,
    /// The new content when it is not what the working tree holds.
    pub new_source: Option<String>,
}

/// Which tests of a file to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestSelection {
    /// Run the whole file rather than `names`.
    pub whole: bool,
    pub names: BTreeSet<String>,
    /// Why the file was selected.
    pub reasons: Vec<String>,
}

/// What a change means for one project.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// The project's language is analysed at source level.
    pub analysed: bool,
    /// The project owns a changed file.
    pub changed: bool,
    /// Run every test, and why.
    pub full: Option<String>,
    /// Some definition in the project is affected.
    pub touched: bool,
    /// The declared project graph reaches the project from a changed one,
    /// as `--dependents` would.
    pub declared: bool,
    /// Selected test files, relative to the project.
    pub tests: BTreeMap<PathBuf, TestSelection>,
    /// `full` comes from the project's own files rather than from a
    /// dependency.
    pub full_own: bool,
    /// Dependencies (project addresses) whose change this project cannot
    /// follow at source level, which is why it runs in full.
    pub full_via: BTreeSet<String>,
    /// The project holds analysed source files.
    pub has_sources: bool,
    /// Something other than test code is affected: what the project builds
    /// or provides may differ. Dependents that cannot be followed at source
    /// level only need to run when this is set.
    pub product: bool,
    /// Dependencies this project consumes without importing them, for which
    /// only the definitions that use them run, and how each was found.
    pub narrowed: Vec<String>,
    /// The reason in `full` says nothing about what dependents observe.
    blind: bool,
}

impl Outcome {
    /// Whether anything in the project needs to run. A project only the
    /// source graph found, outside the declared graph, counts for the tests
    /// it contributes and nothing else.
    pub fn affected(&self) -> bool {
        self.changed
            || self.full.is_some()
            || !self.tests.is_empty()
            || (self.touched && self.declared)
    }
}

/// The result of analysing a change.
#[derive(Debug, Default)]
pub struct Related {
    /// Outcome by project address (`//path`).
    pub projects: HashMap<String, Outcome>,
    pub files_analysed: usize,
    /// Where the analysis spent its time, for `--verbose`.
    pub timings: String,
}

const TOP: u32 = u32::MAX;
/// `(file, definition)`; `TOP` stands for the code outside every definition.
type DefId = (u32, u32);
const NO_DEF: DefId = (u32::MAX, u32::MAX);

struct Source {
    path: PathBuf,
    facts: FileFacts,
    unit: u32,
    project: Option<usize>,
    /// Deleted by the change; present so that its users still resolve.
    ghost: bool,
    /// Units each definition uses; the last entry is the top-level code.
    uses: Vec<Vec<u32>>,
    /// Units whose users reach each definition (see `Def::via`).
    via: Vec<Vec<u32>>,
    /// Qualified calls of each definition, resolved to units; the last
    /// entry is the top-level code.
    calls: Vec<Vec<(u32, String)>>,
    /// Units whose names the whole file can use unqualified.
    imports: Vec<u32>,
}

struct Index<'a> {
    projects: &'a [DiscoveredProject],
    sources: Vec<Source>,
    by_path: HashMap<PathBuf, u32>,
    /// Files of each unit: one file, or every Go file of a directory.
    unit_files: Vec<Vec<u32>>,
    /// Definitions that use each unit.
    users: Vec<Vec<DefId>>,
    /// Definitions that mention each method name.
    method_refs: HashMap<String, Vec<DefId>>,
    /// Definitions that hold a URL path literal.
    path_defs: Vec<DefId>,
    /// For each project, itself and every project that depends on it.
    downstream: Vec<HashSet<usize>>,
    /// For each project, the projects that depend on it directly.
    dependents: Vec<Vec<usize>>,
    /// `(dependent, dependency)` project pairs where the dependent's source
    /// uses a file of the dependency.
    imports: HashSet<(usize, usize)>,
    /// Each project's `[consumes]` entries by dependency.
    consumes: Vec<HashMap<usize, Consume>>,
}

#[derive(Clone)]
enum Why {
    Changed(String),
    Via(DefId),
}

struct Run<'a, 'b> {
    ix: &'b Index<'a>,
    affected: HashMap<DefId, Why>,
    queue: VecDeque<DefId>,
    symbols_seen: HashSet<(u32, String)>,
    users_seen: HashSet<u32>,
    stop_at_test: bool,
    hit_test: bool,
}

/// Analyse `changes` against the workspace at `root`.
pub fn analyse(
    root: &Path,
    projects: &[DiscoveredProject],
    graph: &ProjectGraph,
    changes: &[Change],
) -> Related {
    let started = std::time::Instant::now();
    let (paths, mut facts) = scan(root);
    let scanned = started.elapsed();
    for change in changes {
        let Some(family) = Family::of(&change.path) else {
            continue;
        };
        if let Some(source) = &change.new_source {
            facts.insert(change.path.clone(), extract(family, &change.path, source));
        }
    }
    let mut ghosts = HashSet::new();
    for change in changes {
        if !change.deleted || facts.contains_key(&change.path) {
            continue;
        }
        if let (Some(family), Some(old)) = (Family::of(&change.path), &change.old_source) {
            facts.insert(change.path.clone(), extract(family, &change.path, old));
            ghosts.insert(change.path.clone());
        }
    }
    let files_analysed = facts.len();
    let index = Index::build(root, projects, graph, &paths, facts, &ghosts);
    let indexed = started.elapsed();
    let mut related = index.select(changes);
    related.files_analysed = files_analysed;
    related.timings = format!(
        "parse {:.1?}, link {:.1?}, select {:.1?}",
        scanned,
        indexed - scanned,
        started.elapsed() - indexed
    );
    related
}

fn extract(family: Family, path: &Path, source: &str) -> FileFacts {
    match family {
        Family::Elixir => elixir::extract(path, source),
        Family::Js => js::extract(path, source),
        Family::Go => go::extract(path, source),
        Family::Python => python::extract(path, source),
    }
}

/// Every file of the workspace, and the facts of every source file.
fn scan(root: &Path) -> (HashSet<PathBuf>, HashMap<PathBuf, FileFacts>) {
    let mut paths = HashSet::new();
    let mut sources = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .is_none_or(|name| !SKIPPED_DIRS.contains(&name))
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        if let Some(family) = Family::of(relative) {
            sources.push((relative.to_path_buf(), family));
        }
        paths.insert(relative.to_path_buf());
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(sources.len().max(1));
    let chunk = sources.len().div_ceil(threads).max(1);
    let mut facts = HashMap::with_capacity(sources.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = sources
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(path, family)| {
                            let absolute = root.join(path);
                            let oversized = std::fs::metadata(&absolute)
                                .is_ok_and(|m| m.len() > MAX_SOURCE_BYTES);
                            let source = if oversized {
                                None
                            } else {
                                std::fs::read_to_string(&absolute).ok()
                            };
                            let facts = match source {
                                Some(source) => extract(*family, path, &source),
                                None => {
                                    let mut facts = FileFacts::new(*family);
                                    facts.parse_error = true;
                                    facts
                                }
                            };
                            (path.clone(), facts)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            if let Ok(chunk) = handle.join() {
                facts.extend(chunk);
            }
        }
    });
    (paths, facts)
}

fn address(project: &DiscoveredProject) -> String {
    format!("//{}", project.relative_path.display())
}

/// The most specific project containing `path`.
fn owner(projects: &[DiscoveredProject], path: &Path) -> Option<usize> {
    projects
        .iter()
        .enumerate()
        .filter(|(_, p)| path.starts_with(&p.relative_path))
        .max_by_key(|(_, p)| p.relative_path.as_os_str().len())
        .map(|(i, _)| i)
}

/// Files whose change says nothing about which code is affected: manifests,
/// lockfiles and tool configuration.
fn is_trigger(project: &DiscoveredProject, relative: &Path) -> bool {
    let name = relative.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let path = relative.to_string_lossy().replace('\\', "/");
    if name == "aster.toml"
        || project.config_path.file_name().and_then(|n| n.to_str()) == Some(name)
            && relative.components().count() == 1
        || matches!(name, ".tool-versions" | ".mise.toml" | "mise.toml")
    {
        return true;
    }
    match project.plugin_name.as_str() {
        "nodejs" => {
            matches!(
                name,
                "package.json"
                    | "package-lock.json"
                    | "pnpm-lock.yaml"
                    | "pnpm-workspace.yaml"
                    | "yarn.lock"
                    | "bun.lock"
                    | "bun.lockb"
                    | "bunfig.toml"
                    | ".npmrc"
                    | "jsconfig.json"
                    | ".babelrc"
            ) || (name.starts_with("tsconfig") && name.ends_with(".json"))
                || [
                    "vitest.",
                    "vite.",
                    "jest.config.",
                    "babel.config.",
                    "playwright.config.",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        }
        "elixir" => {
            matches!(name, "mix.exs" | "mix.lock")
                || path == "test/test_helper.exs"
                || (path.starts_with("config/") && name.ends_with(".exs"))
        }
        "go" => {
            matches!(name, "go.mod" | "go.sum" | "go.work" | "go.work.sum")
                || path.starts_with("vendor/")
        }
        "python" => {
            matches!(
                name,
                "pyproject.toml"
                    | "setup.py"
                    | "setup.cfg"
                    | "pytest.ini"
                    | "tox.ini"
                    | "poetry.lock"
                    | "uv.lock"
                    | ".python-version"
                    | "Pipfile"
                    | "Pipfile.lock"
            ) || (name.starts_with("requirements") && name.ends_with(".txt"))
        }
        _ => false,
    }
}

fn is_prose(path: &Path) -> bool {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        extension,
        "md" | "mdx" | "markdown" | "rst" | "adoc" | "txt"
    ) || matches!(
        name,
        "LICENSE" | "NOTICE" | "CODEOWNERS" | "AUTHORS" | "CHANGELOG"
    )
}

/// The plugin whose projects a workspace-level lockfile governs.
fn lockfile_plugin(name: &str) -> Option<&'static str> {
    match name {
        "pnpm-lock.yaml"
        | "pnpm-workspace.yaml"
        | "package-lock.json"
        | "yarn.lock"
        | "bun.lock"
        | "bun.lockb" => Some("nodejs"),
        "go.work" | "go.work.sum" => Some("go"),
        "mix.lock" => Some("elixir"),
        "uv.lock" | "poetry.lock" => Some("python"),
        "Cargo.lock" => Some("rust"),
        _ => None,
    }
}

fn spans(lines: &Option<Vec<(usize, usize)>>) -> String {
    match lines {
        Some(lines) if !lines.is_empty() => {
            let (start, end) = lines[0];
            let more = if lines.len() > 1 { ", …" } else { "" };
            if start == end {
                format!(":{start}{more}")
            } else {
                format!(":{start}-{end}{more}")
            }
        }
        _ => String::new(),
    }
}

impl<'a> Index<'a> {
    fn build(
        root: &Path,
        projects: &'a [DiscoveredProject],
        graph: &ProjectGraph,
        paths: &HashSet<PathBuf>,
        facts: HashMap<PathBuf, FileFacts>,
        ghosts: &HashSet<PathBuf>,
    ) -> Self {
        let ws = Workspace {
            root,
            paths,
            sources: &facts,
        };
        let resolvers = Resolvers::build(&ws);

        let mut ordered: Vec<&PathBuf> = facts.keys().collect();
        ordered.sort();
        let by_path: HashMap<PathBuf, u32> = ordered
            .iter()
            .enumerate()
            .map(|(i, path)| ((*path).clone(), i as u32))
            .collect();

        // Units: a Go package is its directory; elsewhere a unit is a file.
        let mut unit_ids: HashMap<PathBuf, u32> = HashMap::new();
        let mut unit_files: Vec<Vec<u32>> = Vec::new();
        let mut file_unit = Vec::with_capacity(ordered.len());
        for (i, path) in ordered.iter().enumerate() {
            let key = if facts[*path].family == Family::Go {
                path.parent().unwrap_or(Path::new("")).to_path_buf()
            } else {
                (*path).clone()
            };
            let unit = *unit_ids.entry(key).or_insert_with(|| {
                unit_files.push(Vec::new());
                (unit_files.len() - 1) as u32
            });
            unit_files[unit as usize].push(i as u32);
            file_unit.push(unit);
        }
        // Go specifiers resolve to a directory, the others to a file; both
        // are unit keys.
        let unit_of = |path: &Path| -> Option<u32> { unit_ids.get(path).copied() };
        let resolve = |from: &Path, family: Family, spec: &str| -> Vec<u32> {
            resolvers
                .resolve(&ws, from, family, spec)
                .iter()
                .filter_map(|target| unit_of(target))
                .collect()
        };

        // Units each unit re-exports.
        let mut reexports: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, path) in ordered.iter().enumerate() {
            let file = &facts[*path];
            for spec in &file.reexports {
                reexports
                    .entry(file_unit[i])
                    .or_default()
                    .extend(resolve(path, file.family, spec));
            }
        }
        let expand = |direct: BTreeSet<u32>| -> Vec<u32> {
            let mut seen = direct.clone();
            let mut stack: Vec<u32> = direct.into_iter().collect();
            while let Some(unit) = stack.pop() {
                for next in reexports.get(&unit).into_iter().flatten() {
                    if seen.insert(*next) {
                        stack.push(*next);
                    }
                }
            }
            seen.into_iter().collect()
        };

        let mut sources = Vec::with_capacity(ordered.len());
        for (i, path) in ordered.iter().enumerate() {
            let file = &facts[*path];
            let family = file.family;
            let mut shared: BTreeSet<u32> = BTreeSet::new();
            for spec in &file.uses_all {
                shared.extend(resolve(path, family, spec));
            }
            if family == Family::Python {
                for conftest in resolvers.python_implicit(&ws, path) {
                    shared.extend(unit_of(&conftest));
                }
            }
            let go_imports: Vec<(String, Option<u32>)> = file
                .go_imports
                .iter()
                .map(|(alias, import)| {
                    let unit = resolvers.go_dir(import).and_then(|dir| unit_of(&dir));
                    (resolvers.go_qualifier(import, alias.as_deref()), unit)
                })
                .collect();
            let uses_of = |def: &super::facts::Def, top: bool| -> Vec<u32> {
                let mut units = shared.clone();
                for spec in &def.uses {
                    units.extend(resolve(path, family, spec));
                }
                for (qualifier, unit) in &go_imports {
                    let Some(unit) = unit else { continue };
                    let used = match qualifier.as_str() {
                        // Imported for its side effects.
                        "_" => top,
                        "." => true,
                        name => def.refs.contains(name),
                    };
                    if used {
                        units.insert(*unit);
                    }
                }
                units.remove(&file_unit[i]);
                expand(units)
            };
            let mut uses: Vec<Vec<u32>> = file.defs.iter().map(|d| uses_of(d, false)).collect();
            uses.push(uses_of(&file.top, true));
            let via: Vec<Vec<u32>> = file
                .defs
                .iter()
                .map(|def| {
                    def.via
                        .iter()
                        .flat_map(|spec| resolve(path, family, spec))
                        .filter(|unit| *unit != file_unit[i])
                        .collect()
                })
                .collect();
            let calls_of = |def: &super::facts::Def| -> Vec<(u32, String)> {
                def.calls
                    .iter()
                    .flat_map(|(module, function)| {
                        resolve(path, family, module)
                            .into_iter()
                            .map(move |unit| (unit, function.clone()))
                    })
                    .collect()
            };
            let mut calls: Vec<Vec<(u32, String)>> = file.defs.iter().map(calls_of).collect();
            calls.push(calls_of(&file.top));
            let imports: Vec<u32> = shared.iter().copied().collect();
            sources.push((uses, via, calls, imports, file_unit[i]));
        }

        let mut facts = facts;
        let sources: Vec<Source> = ordered_owned(&by_path)
            .into_iter()
            .zip(sources)
            .map(|(path, (uses, via, calls, imports, unit))| Source {
                facts: facts.remove(&path).expect("facts for every indexed path"),
                project: owner(projects, &path),
                ghost: ghosts.contains(&path),
                path,
                unit,
                uses,
                via,
                calls,
                imports,
            })
            .collect();

        let mut users: Vec<Vec<DefId>> = vec![Vec::new(); unit_files.len()];
        let mut methods: HashSet<&str> = HashSet::new();
        for source in &sources {
            for def in &source.facts.defs {
                if def.kind == DefKind::Method {
                    methods.insert(&def.name);
                }
            }
        }
        let mut path_defs = Vec::new();
        for (f, source) in sources.iter().enumerate() {
            for (d, def) in source.facts.defs.iter().enumerate() {
                if !def.paths.is_empty() {
                    path_defs.push((f as u32, d as u32));
                }
            }
            if !source.facts.top.paths.is_empty() {
                path_defs.push((f as u32, TOP));
            }
        }
        let mut method_refs: HashMap<String, Vec<DefId>> = HashMap::new();
        for (f, source) in sources.iter().enumerate() {
            for (d, units) in source.uses.iter().enumerate() {
                let id = if d == source.facts.defs.len() {
                    (f as u32, TOP)
                } else {
                    (f as u32, d as u32)
                };
                for unit in units {
                    users[*unit as usize].push(id);
                }
                let def = if id.1 == TOP {
                    &source.facts.top
                } else {
                    &source.facts.defs[d]
                };
                for name in &def.refs {
                    if methods.contains(name.as_str()) {
                        method_refs.entry(name.clone()).or_default().push(id);
                    }
                }
            }
        }

        let index_of: HashMap<String, usize> = projects
            .iter()
            .enumerate()
            .map(|(i, p)| (address(p), i))
            .collect();
        let downstream = projects
            .iter()
            .enumerate()
            .map(|(i, project)| {
                let mut seen = HashSet::from([i]);
                let mut queue = VecDeque::from([address(project)]);
                while let Some(current) = queue.pop_front() {
                    for dependent in graph.dependents(&current) {
                        if let Some(&j) = index_of.get(&dependent) {
                            if seen.insert(j) {
                                queue.push_back(dependent);
                            }
                        }
                    }
                }
                seen
            })
            .collect();

        let dependents = projects
            .iter()
            .map(|project| {
                graph
                    .dependents(&address(project))
                    .iter()
                    .filter_map(|dependent| index_of.get(dependent).copied())
                    .collect()
            })
            .collect();

        let mut imports = HashSet::new();
        for source in &sources {
            let Some(dependent) = source.project else {
                continue;
            };
            for unit in source.uses.iter().flatten() {
                for &file in &unit_files[*unit as usize] {
                    if let Some(dependency) = sources[file as usize].project {
                        imports.insert((dependent, dependency));
                    }
                }
            }
        }

        Index {
            projects,
            consumes: consumers::load(projects),
            imports,
            dependents,
            sources,
            by_path,
            unit_files,
            users,
            method_refs,
            path_defs,
            downstream,
        }
    }

    /// Whether `id` is a definition of a test file.
    fn in_test_file(&self, id: DefId) -> bool {
        id != NO_DEF && self.sources[id.0 as usize].facts.is_test_file
    }

    /// Whether definition `id` refers to `name` of `unit`. The caller has
    /// already established that `id` uses `unit` or belongs to it.
    fn refers(&self, id: DefId, unit: u32, name: &str) -> bool {
        let source = &self.sources[id.0 as usize];
        let def = self.def(id);
        if !source.facts.qualified {
            return def.refs.contains(name);
        }
        let index = if id.1 == TOP {
            source.facts.defs.len()
        } else {
            id.1 as usize
        };
        // A qualified call to that module, a name that may be dispatched
        // on at run time, or a bare name where bare names can mean it.
        source.calls[index]
            .iter()
            .any(|(called, function)| *called == unit && function == name)
            || def.atoms.contains(name)
            || (def.refs.contains(name) && (source.unit == unit || source.imports.contains(&unit)))
    }

    fn def(&self, id: DefId) -> &super::facts::Def {
        let facts = &self.sources[id.0 as usize].facts;
        if id.1 == TOP {
            &facts.top
        } else {
            &facts.defs[id.1 as usize]
        }
    }

    fn describe(&self, id: DefId) -> String {
        let source = &self.sources[id.0 as usize];
        if id.1 == TOP {
            return source.path.display().to_string();
        }
        let def = &source.facts.defs[id.1 as usize];
        format!("{} ({}:{})", def.name, source.path.display(), def.lines.0)
    }

    /// The chain of reasons from `id` back to the change.
    fn explain(&self, run: &Run, id: DefId) -> String {
        let mut parts = Vec::new();
        let mut current = id;
        for _ in 0..64 {
            match run.affected.get(&current) {
                Some(Why::Changed(what)) => {
                    parts.push(what.clone());
                    break;
                }
                Some(Why::Via(parent)) if *parent != NO_DEF => {
                    // The file's own top-level code adds nothing to a chain.
                    if !(parent.1 == TOP && parent.0 == current.0) {
                        parts.push(self.describe(*parent));
                    }
                    current = *parent;
                }
                _ => break,
            }
        }
        parts.dedup();
        if parts.len() > 4 {
            let last = parts.pop().unwrap_or_default();
            parts.truncate(2);
            parts.push("…".to_string());
            parts.push(last);
        }
        parts.join(" ← ")
    }

    fn select(&self, changes: &[Change]) -> Related {
        let mut outcomes: Vec<Outcome> = self
            .projects
            .iter()
            .map(|p| Outcome {
                analysed: Family::of_plugin(&p.plugin_name).is_some(),
                ..Outcome::default()
            })
            .collect();
        let set_full =
            |outcomes: &mut Vec<Outcome>, project: usize, blind: bool, reason: String| {
                let outcome = &mut outcomes[project];
                if outcome.full.is_none() || (blind && !outcome.blind) {
                    outcome.full = Some(reason);
                }
                outcome.blind |= blind;
            };

        let mut run = Run::new(self, false);
        // Changed sources that belong to their project's own language, with
        // the project that owns them.
        let mut own_sources: Vec<(&Change, usize)> = Vec::new();
        let mut data: Vec<(&Change, usize)> = Vec::new();

        for change in changes {
            let project = owner(self.projects, &change.path);
            let name = change
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if let Some(plugin) = lockfile_plugin(name) {
                let dir = change.path.parent().unwrap_or(Path::new(""));
                for (i, candidate) in self.projects.iter().enumerate() {
                    if candidate.plugin_name == plugin && candidate.relative_path.starts_with(dir) {
                        outcomes[i].changed = true;
                        outcomes[i].product = true;
                        let reason = format!("{} changed", change.path.display());
                        set_full(&mut outcomes, i, true, reason);
                    }
                }
            }
            if let Some(p) = project {
                outcomes[p].changed = true;
                // Sources count through the definitions they affect; any
                // other file may be part of what the project provides.
                if !self.by_path.contains_key(&change.path) && !is_prose(&change.path) {
                    outcomes[p].product = true;
                }
                let relative = change
                    .path
                    .strip_prefix(&self.projects[p].relative_path)
                    .unwrap_or(&change.path);
                if is_trigger(&self.projects[p], relative) {
                    let reason = format!("{} changed", change.path.display());
                    set_full(&mut outcomes, p, true, reason);
                    continue;
                }
            }
            let indexed = self.by_path.get(&change.path).copied();
            let plugin_family =
                project.and_then(|p| Family::of_plugin(&self.projects[p].plugin_name));
            match indexed {
                Some(file) => {
                    run.seed(change, file);
                    let family = self.sources[file as usize].facts.family;
                    match project {
                        Some(p) if plugin_family == Some(family) => own_sources.push((change, p)),
                        // A source file of another language is data to the
                        // project that holds it.
                        Some(p) if outcomes[p].analysed => data.push((change, p)),
                        _ => {}
                    }
                }
                None => {
                    if let Some(p) = project.filter(|p| outcomes[*p].analysed) {
                        data.push((change, p));
                    }
                }
            }
        }

        // Files named by string literals, wherever the naming code lives.
        let mentioned = self.seed_mentions(&mut run, changes);
        for (change, project) in data {
            if mentioned.contains(&change.path) {
                continue;
            }
            // Prose that no source names cannot change what a test sees.
            if is_prose(&change.path) {
                continue;
            }
            let reason = format!("{} is not named by any source file", change.path.display());
            set_full(&mut outcomes, project, false, reason);
        }
        run.drain();

        for (change, project) in &own_sources {
            if outcomes[*project].full.is_some() {
                continue;
            }
            let file = self.by_path[&change.path];
            let source = &self.sources[file as usize];
            let dynamic = self
                .sources
                .iter()
                .filter(|s| s.project == Some(*project))
                .find_map(|s| s.facts.dynamic);
            if let Some(dynamic) = dynamic {
                let reason = format!("the project {dynamic}");
                set_full(&mut outcomes, *project, false, reason);
                continue;
            }
            if source.facts.is_test_file && !source.ghost {
                continue;
            }
            // A change that reaches no test at all is not understood.
            let mut probe = Run::new(self, true);
            probe.seed(change, file);
            probe.drain();
            if !probe.affected.is_empty() && !probe.hit_test {
                let reason = format!("{} reaches no test", change.path.display());
                set_full(&mut outcomes, *project, false, reason);
            }
        }

        let base_product: Vec<bool> = outcomes.iter().map(|o| o.product).collect();
        self.spread(&mut run, &mut outcomes, &base_product);

        Related {
            projects: self
                .projects
                .iter()
                .zip(outcomes)
                .map(|(project, outcome)| (address(project), outcome))
                .collect(),
            files_analysed: 0,
            timings: String::new(),
        }
    }

    /// Whether a source file only supports tests (`test/support`, fixtures,
    /// `__tests__` helpers): what a project provides to its dependents does
    /// not change with it.
    fn test_side(&self, source: &Source) -> bool {
        if source.facts.is_test_file {
            return true;
        }
        let relative = source
            .project
            .and_then(|p| {
                source
                    .path
                    .strip_prefix(&self.projects[p].relative_path)
                    .ok()
            })
            .unwrap_or(&source.path);
        let mut parts = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy());
        parts
            .next()
            .is_some_and(|first| matches!(first.as_ref(), "test" | "tests" | "spec"))
            || relative.components().any(|c| c.as_os_str() == "__tests__")
    }

    /// Derive what the affected definitions mean for each project: whether
    /// it is touched, whether its product changed, and the tests selected.
    fn collect(&self, run: &Run, outcomes: &mut [Outcome], base_product: &[bool], forced: &[bool]) {
        for (i, outcome) in outcomes.iter_mut().enumerate() {
            outcome.touched = false;
            outcome.product = base_product[i] || forced[i];
            outcome.tests.clear();
        }
        let mut ids: Vec<&DefId> = run.affected.keys().collect();
        ids.sort();
        for id in ids {
            let source = &self.sources[id.0 as usize];
            let Some(project) = source.project else {
                continue;
            };
            if source.ghost {
                continue;
            }
            outcomes[project].touched = true;
            if !source.facts.is_test_file {
                if !self.test_side(source) {
                    outcomes[project].product = true;
                }
                continue;
            }
            // A test in another language is not something the project's
            // test runner can be narrowed to; a command that is not a known
            // runner still runs as written because the project is touched.
            let plugin_family = Family::of_plugin(&self.projects[project].plugin_name);
            if plugin_family != Some(source.facts.family) {
                continue;
            }
            let relative = source
                .path
                .strip_prefix(&self.projects[project].relative_path)
                .unwrap_or(&source.path)
                .to_path_buf();
            let selection = outcomes[project].tests.entry(relative).or_default();
            if id.1 == TOP {
                selection.whole = true;
            } else if self.def(*id).is_test() {
                selection.names.insert(self.def(*id).name.clone());
            }
            let reason = self.explain(run, *id);
            if !reason.is_empty() && !selection.reasons.contains(&reason) {
                selection.reasons.push(reason);
            }
        }
        for outcome in outcomes.iter_mut() {
            for selection in outcome.tests.values_mut() {
                let extra = selection.reasons.len().saturating_sub(MAX_REASONS);
                selection.reasons.truncate(MAX_REASONS);
                if extra > 0 {
                    selection.reasons.push(format!("and {extra} more"));
                }
            }
        }
    }

    /// Carry changes across project edges the source graph cannot see: into
    /// and out of projects that are not analysed, across languages, and from
    /// projects that run in full for a reason that hides what changed.
    ///
    /// A dependent that declares which of its sources consume the
    /// dependency runs those and the tests that reach them instead of
    /// running in full; what they affect carries on to its own dependents.
    fn spread(&self, run: &mut Run<'a, '_>, outcomes: &mut [Outcome], base_product: &[bool]) {
        let family = |i: usize| Family::of_plugin(&self.projects[i].plugin_name);
        // Projects the declared graph reaches from a changed project: what
        // `--dependents` would select. Opaque edges are followed inside this
        // set only; a project the source graph alone found contributes its
        // affected tests and nothing more.
        let declared: HashSet<usize> = outcomes
            .iter()
            .enumerate()
            .filter(|(_, outcome)| outcome.changed)
            .flat_map(|(i, _)| self.downstream[i].iter().copied())
            .collect();
        for &project in &declared {
            outcomes[project].declared = true;
        }
        for (i, outcome) in outcomes.iter_mut().enumerate() {
            outcome.full_own = outcome.full.is_some();
            outcome.has_sources = self
                .sources
                .iter()
                .any(|s| s.project == Some(i) && !s.ghost);
        }
        let mut forced = vec![false; outcomes.len()];
        let mut handled: HashSet<(usize, usize)> = HashSet::new();
        loop {
            self.collect(run, outcomes, base_product, &forced);
            let mut updates: Vec<(usize, usize, String, String)> = Vec::new();
            for (dependency, outcome) in outcomes.iter().enumerate() {
                // A project whose tests alone are affected builds and
                // behaves as before; nothing downstream can tell.
                if !outcome.affected() || !outcome.declared || !outcome.product {
                    continue;
                }
                let opaque = outcome.blind || !outcome.analysed;
                let name = address(&self.projects[dependency]);
                for &dependent in &self.dependents[dependency] {
                    if dependent == dependency
                        || outcomes[dependent].full_via.contains(&name)
                        || handled.contains(&(dependent, dependency))
                    {
                        continue;
                    }
                    let reason = if opaque {
                        format!("depends on {name}, whose change is not analysed")
                    } else if family(dependent) != family(dependency) {
                        format!("depends on {name}, which is in another language")
                    } else if !self.imports.contains(&(dependent, dependency)) {
                        // A declared dependency with no import behind it is
                        // on something other than source: a built artifact,
                        // a binary the tests run.
                        format!("depends on {name} without importing it")
                    } else {
                        continue;
                    };
                    updates.push((dependent, dependency, name.clone(), reason));
                }
            }
            if updates.is_empty() {
                break;
            }
            for (dependent, dependency, name, mut reason) in updates {
                handled.insert((dependent, dependency));
                if outcomes[dependent].full.is_none() {
                    match self.consumer_seeds(dependent, dependency, &outcomes[dependency]) {
                        Ok((seeds, note)) => {
                            for (id, why) in seeds {
                                run.mark(id, Why::Changed(why));
                            }
                            outcomes[dependent]
                                .narrowed
                                .push(format!("{reason}; {note}"));
                            continue;
                        }
                        Err(missing) => reason.push_str(&missing),
                    }
                }
                let outcome = &mut outcomes[dependent];
                outcome.full_via.insert(name);
                forced[dependent] = true;
                if !outcome.blind {
                    outcome.full = Some(reason);
                    outcome.blind = true;
                }
            }
            run.drain();
        }
    }

    /// The definitions of `dependent` that consume `dependency`, as far as
    /// its `[consumes]` entry and the sources show, with a note saying how
    /// they were found. `Err` carries the reason there is no usable
    /// evidence, to append to why the project runs in full.
    #[allow(clippy::type_complexity)]
    fn consumer_seeds(
        &self,
        dependent: usize,
        dependency: usize,
        upstream: &Outcome,
    ) -> Result<(Vec<(DefId, String)>, String), String> {
        let name = address(&self.projects[dependency]);
        let state = if upstream.changed {
            "changed"
        } else {
            "is affected"
        };
        let Some(entry) = self.consumes[dependent].get(&dependency) else {
            let named = self.naming_defs(dependent, dependency).len();
            return Err(if named > 0 {
                format!(
                    "; {named} definition(s) of the project name a path in {name}; \
                     `[consumes.\"{name}\"] infer = true` in its aster.toml runs only those"
                )
            } else {
                String::new()
            });
        };
        let mut seeds: Vec<(DefId, String)> = Vec::new();
        let mut how = Vec::new();
        if let Some(patterns) = &entry.files {
            let dir = &self.projects[dependent].relative_path;
            for pattern in patterns {
                let (base, glob) = match pattern.strip_prefix("//") {
                    Some(rest) => (PathBuf::new(), rest),
                    None => (dir.clone(), pattern.as_str()),
                };
                let Ok(glob) = globset::Glob::new(glob) else {
                    continue;
                };
                let matcher = glob.compile_matcher();
                let mut matched = false;
                for (f, source) in self.sources.iter().enumerate() {
                    if source.ghost {
                        continue;
                    }
                    let Ok(relative) = source.path.strip_prefix(&base) else {
                        continue;
                    };
                    if matcher.is_match(relative) {
                        matched = true;
                        seeds.push((
                            (f as u32, TOP),
                            format!(
                                "aster.toml declares {} to consume {name}",
                                source.path.display()
                            ),
                        ));
                    }
                }
                if !matched {
                    return Err(format!(
                        "; `[consumes.\"{name}\"]` lists {pattern}, which matches no source file"
                    ));
                }
            }
            how.push(format!("{} declared source file(s)", seeds.len()));
        }
        if entry.infer {
            let named = self.naming_defs(dependent, dependency);
            how.push(format!(
                "{} definition(s) naming a path in {name}",
                named.len()
            ));
            for (id, path) in named {
                seeds.push((
                    id,
                    format!("names {} in {name}, which {state}", path.display()),
                ));
            }
        }
        if seeds.is_empty() {
            if entry.files.is_some() && !entry.infer {
                return Ok((
                    seeds,
                    format!("aster.toml declares that no source consumes {name}"),
                ));
            }
            return Err(format!(
                "; no definition of the project names a path in {name}"
            ));
        }
        // Evidence nothing tests is not understood.
        let mut probe = Run::new(self, true);
        for (id, why) in &seeds {
            probe.mark(*id, Why::Changed(why.clone()));
        }
        probe.drain();
        if !probe.hit_test {
            return Err(format!("; the sources that consume {name} reach no test"));
        }
        Ok((
            seeds,
            format!("runs the tests reaching {}", how.join(" and ")),
        ))
    }

    /// Definitions of `dependent` whose string literals name a path that
    /// `dependency` owns, with the first such path.
    fn naming_defs(&self, dependent: usize, dependency: usize) -> Vec<(DefId, PathBuf)> {
        let target = &self.projects[dependency].relative_path;
        if target.as_os_str().is_empty() {
            return Vec::new();
        }
        let project_dir = &self.projects[dependent].relative_path;
        let mut found = Vec::new();
        for (f, source) in self.sources.iter().enumerate() {
            if source.project != Some(dependent) || source.ghost {
                continue;
            }
            let file_dir = source.path.parent().unwrap_or(Path::new(""));
            let defs = source
                .facts
                .defs
                .iter()
                .enumerate()
                .map(|(d, def)| ((f as u32, d as u32), def))
                .chain(std::iter::once(((f as u32, TOP), &source.facts.top)));
            for (id, def) in defs {
                let named = def.strings.iter().find_map(|string| {
                    consumers::named_paths(string, file_dir, project_dir)
                        .into_iter()
                        .find(|path| owner(self.projects, path) == Some(dependency))
                });
                if let Some(path) = named {
                    found.push((id, path));
                }
            }
        }
        found
    }

    /// Seed the definitions whose string literals name a changed file.
    /// Returns the changed files something names.
    fn seed_mentions(&self, run: &mut Run, changes: &[Change]) -> HashSet<PathBuf> {
        // A source file in its own project's language is followed by
        // reference; only code in another language can depend on it by path.
        let native = |change: &Change| {
            self.by_path.get(&change.path).and_then(|&file| {
                let family = self.sources[file as usize].facts.family;
                owner(self.projects, &change.path)
                    .is_none_or(|p| {
                        Family::of_plugin(&self.projects[p].plugin_name) == Some(family)
                    })
                    .then_some(family)
            })
        };
        let mut by_name: HashMap<&str, Vec<&Change>> = HashMap::new();
        for change in changes {
            if let Some(name) = change.path.file_name().and_then(|n| n.to_str()) {
                by_name.entry(name).or_default().push(change);
            }
        }
        let mut mentioned = HashSet::new();
        if by_name.is_empty() {
            return mentioned;
        }
        for (f, source) in self.sources.iter().enumerate() {
            if source.ghost {
                continue;
            }
            let defs = source
                .facts
                .defs
                .iter()
                .enumerate()
                .map(|(d, def)| ((f as u32, d as u32), def))
                .chain(std::iter::once(((f as u32, TOP), &source.facts.top)));
            for (id, def) in defs {
                for string in &def.strings {
                    for token in string.split(|c: char| {
                        !(c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '@' | '+' | '-'))
                    }) {
                        let name = token.rsplit('/').next().unwrap_or(token);
                        let Some(candidates) = by_name.get(name) else {
                            continue;
                        };
                        let named: Vec<&str> = token
                            .split('/')
                            .filter(|s| !s.is_empty() && *s != "." && *s != "..")
                            .collect();
                        for change in candidates {
                            let actual: Vec<_> = change
                                .path
                                .components()
                                .map(|c| c.as_os_str().to_string_lossy())
                                .collect();
                            // Reading a source file as data takes a path, not
                            // a bare file name.
                            let own_family = native(change);
                            if own_family == Some(source.facts.family)
                                || (own_family.is_some() && named.len() < 2)
                            {
                                continue;
                            }
                            // A bare file name only means the file beside
                            // the code; other projects name it by path.
                            if named.len() < 2
                                && source.project != owner(self.projects, &change.path)
                            {
                                continue;
                            }
                            let matches = named.len() <= actual.len()
                                && named
                                    .iter()
                                    .rev()
                                    .zip(actual.iter().rev())
                                    .all(|(a, b)| *a == b.as_ref());
                            if matches {
                                mentioned.insert(change.path.clone());
                                run.mark(
                                    id,
                                    Why::Changed(format!("{} changed", change.path.display())),
                                );
                            }
                        }
                    }
                }
            }
        }
        // Conventions that tie a data file to tests without naming it.
        for change in by_name.values().flatten() {
            if native(change).is_some() {
                continue;
            }
            let parts: Vec<String> = change
                .path
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let why = || Why::Changed(format!("{} changed", change.path.display()));
            if let Some(at) = parts.iter().position(|p| p == "testdata") {
                let dir: PathBuf = parts[..at].iter().collect();
                for (f, source) in self.sources.iter().enumerate() {
                    if source.facts.family == Family::Go
                        && source.facts.is_test_file
                        && source.path.parent() == Some(dir.as_path())
                    {
                        mentioned.insert(change.path.clone());
                        run.mark((f as u32, TOP), why());
                    }
                }
            }
            // Files a Go declaration embeds.
            for (f, source) in self.sources.iter().enumerate() {
                let Some(relative) = source
                    .path
                    .parent()
                    .and_then(|dir| change.path.strip_prefix(dir).ok())
                else {
                    continue;
                };
                for (pattern, owner) in &source.facts.embeds {
                    let Ok(glob) = globset::GlobBuilder::new(pattern)
                        .literal_separator(true)
                        .build()
                    else {
                        continue;
                    };
                    let matcher = glob.compile_matcher();
                    // A pattern names a file, or a directory embedded whole.
                    if relative
                        .ancestors()
                        .any(|prefix| !prefix.as_os_str().is_empty() && matcher.is_match(prefix))
                    {
                        mentioned.insert(change.path.clone());
                        let def = match owner {
                            super::facts::Owner::Def(d) => *d as u32,
                            super::facts::Owner::Top => TOP,
                        };
                        run.mark((f as u32, def), why());
                    }
                }
            }
            if let Some(at) = parts.iter().position(|p| p == "__snapshots__") {
                let test = parts.last().and_then(|name| name.strip_suffix(".snap"));
                if let (Some(test), true) = (test, at + 2 == parts.len()) {
                    let path: PathBuf = parts[..at].iter().collect::<PathBuf>().join(test);
                    if let Some(&f) = self.by_path.get(&path) {
                        mentioned.insert(change.path.clone());
                        run.mark((f, TOP), why());
                    }
                }
            }
        }
        mentioned
    }
}

fn ordered_owned(by_path: &HashMap<PathBuf, u32>) -> Vec<PathBuf> {
    let mut ordered: Vec<(&PathBuf, &u32)> = by_path.iter().collect();
    ordered.sort_by_key(|(_, index)| **index);
    ordered.into_iter().map(|(path, _)| path.clone()).collect()
}

impl<'a, 'b> Run<'a, 'b> {
    fn new(ix: &'b Index<'a>, stop_at_test: bool) -> Self {
        Run {
            ix,
            affected: HashMap::new(),
            queue: VecDeque::new(),
            symbols_seen: HashSet::new(),
            users_seen: HashSet::new(),
            stop_at_test,
            hit_test: false,
        }
    }

    fn mark(&mut self, id: DefId, why: Why) {
        if self.affected.contains_key(&id) {
            return;
        }
        let source = &self.ix.sources[id.0 as usize];
        if source.facts.is_test_file && !source.ghost {
            self.hit_test = true;
        }
        self.affected.insert(id, why);
        self.queue.push_back(id);
    }

    /// Seed the definitions `change` touches in `file`.
    fn seed(&mut self, change: &Change, file: u32) {
        let ix = self.ix;
        let source = &ix.sources[file as usize];
        let facts = &source.facts;
        let changed = |lines: &Option<Vec<(usize, usize)>>| {
            Why::Changed(format!("{}{} changed", change.path.display(), spans(lines)))
        };
        let whole = change.deleted || change.new_lines.is_none() || facts.parse_error;
        if whole {
            self.mark((file, TOP), changed(&None));
            return;
        }
        for &(start, end) in change.new_lines.iter().flatten() {
            for line in start..=end {
                let span = Some(vec![(line, line)]);
                match facts.class_of(line) {
                    LineClass::Inert => {}
                    LineClass::Top => self.mark((file, TOP), changed(&span)),
                    LineClass::Def(def) => self.mark((file, def), changed(&span)),
                    LineClass::Import(import) => {
                        self.seed_import(file, &facts.imports[import as usize], changed(&span));
                    }
                }
            }
        }
        // Lines the change removed, read against the old file.
        let Some(old_lines) = change.old_lines.as_ref().filter(|l| !l.is_empty()) else {
            return;
        };
        let Some(old) = change
            .old_source
            .as_ref()
            .map(|old| extract(facts.family, &change.path, old))
        else {
            self.mark((file, TOP), changed(&None));
            return;
        };
        for &(start, end) in old_lines {
            for line in start..=end {
                match old.class_of(line) {
                    LineClass::Inert => {}
                    LineClass::Top => self.mark((file, TOP), changed(&None)),
                    LineClass::Import(import) => {
                        self.seed_import(file, &old.imports[import as usize], changed(&None));
                    }
                    LineClass::Def(def) => {
                        let removed = &old.defs[def as usize];
                        let mut survives = false;
                        for (d, current) in facts.defs.iter().enumerate() {
                            if current.name == removed.name {
                                survives = true;
                                self.mark((file, d as u32), changed(&None));
                            }
                        }
                        if !survives {
                            // The definition is gone; whoever still names
                            // it is affected.
                            let member = removed.kind == DefKind::Method;
                            self.symbol(source.unit, &removed.name, member, NO_DEF, file);
                            if removed.callback || removed.kind == DefKind::Macro {
                                self.users(source.unit, NO_DEF);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Seed the definitions of `file` that use what a changed import
    /// brings in. An import nothing uses is there for its side effects, so
    /// it counts as file-level code.
    fn seed_import(&mut self, file: u32, import: &super::facts::ImportLine, why: Why) {
        if import.structural {
            return;
        }
        let ix = self.ix;
        let facts = &ix.sources[file as usize].facts;
        let uses = |def: &super::facts::Def| {
            import.names.iter().any(|name| def.refs.contains(name))
                || import.specs.iter().any(|spec| def.uses.contains(spec))
        };
        let mut users: Vec<u32> = facts
            .defs
            .iter()
            .enumerate()
            .filter(|(_, def)| uses(def))
            .map(|(d, _)| d as u32)
            .collect();
        if users.is_empty() || uses(&facts.top) {
            users = vec![TOP];
        }
        for def in users {
            self.mark((file, def), why.clone());
        }
    }

    fn drain(&mut self) {
        while let Some(id) = self.queue.pop_front() {
            if self.stop_at_test && self.hit_test {
                self.queue.clear();
                return;
            }
            let ix = self.ix;
            let source = &ix.sources[id.0 as usize];
            if id.1 == TOP {
                for d in 0..source.facts.defs.len() {
                    self.mark((id.0, d as u32), Why::Via(id));
                }
                self.users(source.unit, id);
                continue;
            }
            let def = &source.facts.defs[id.1 as usize];
            if let Some(route) = &def.route {
                // A route is reached by requests for its path. When no code
                // names the path, anything that goes through the router
                // may reach it.
                if !self.requests(route, id) {
                    self.users(source.unit, id);
                }
                continue;
            }
            let member = def.kind == DefKind::Method;
            let mut referenced = self.symbol(source.unit, &def.name, member, id, id.0);
            for alias in &def.aliases {
                referenced |= self.symbol(source.unit, alias, member, id, id.0);
            }
            // Nothing names it, so something reaches it another way: a
            // framework callback, a macro expansion, reflection.
            let unnamed = !referenced && !def.is_test() && !source.facts.is_test_file;
            if def.callback || def.kind == DefKind::Macro || unnamed {
                self.users(source.unit, id);
            }
            for unit in &source.via[id.1 as usize] {
                self.users(*unit, id);
            }
        }
    }

    /// Mark every definition holding a path literal that `route` could
    /// serve, in the route's project and the projects downstream of it.
    /// Returns whether any definition does.
    fn requests(&mut self, route: &str, from: DefId) -> bool {
        let ix = self.ix;
        let origin = &ix.sources[from.0 as usize];
        let reach = origin.project.map(|p| &ix.downstream[p]);
        let mut found = false;
        for &candidate in &ix.path_defs {
            let other = &ix.sources[candidate.0 as usize];
            let in_reach = match (reach, other.project) {
                (Some(reach), Some(project)) => reach.contains(&project),
                _ => true,
            };
            if candidate == from || other.facts.family != origin.facts.family || !in_reach {
                continue;
            }
            if ix
                .def(candidate)
                .paths
                .iter()
                .any(|literal| super::facts::path_reaches(literal, route))
            {
                found = true;
                self.mark(candidate, Why::Via(from));
            }
        }
        found
    }

    /// Mark every definition that uses `unit`.
    fn users(&mut self, unit: u32, from: DefId) {
        if !self.users_seen.insert(unit) {
            return;
        }
        let ix = self.ix;
        let from_test = ix.in_test_file(from);
        for &user in &ix.users[unit as usize] {
            if !from_test || ix.sources[user.0 as usize].facts.is_test_file {
                self.mark(user, Why::Via(from));
            }
        }
    }

    /// Mark every definition that refers to `name` of `unit`. Returns
    /// whether any definition does.
    fn symbol(&mut self, unit: u32, name: &str, member: bool, from: DefId, file: u32) -> bool {
        if !self.symbols_seen.insert((unit, name.to_string())) {
            return true;
        }
        let ix = self.ix;
        let mut found = false;
        let local = ix.unit_files[unit as usize].iter().flat_map(|&f| {
            let defs = ix.sources[f as usize].facts.defs.len() as u32;
            (0..defs).map(move |d| (f, d)).chain([(f, TOP)])
        });
        let candidates: Vec<DefId> = ix.users[unit as usize]
            .iter()
            .copied()
            .chain(local)
            .collect();
        // Nothing but other tests builds on a test file.
        let from_test = ix.sources[file as usize].facts.is_test_file;
        for candidate in candidates {
            if from_test && !ix.sources[candidate.0 as usize].facts.is_test_file {
                continue;
            }
            if candidate != from && ix.refers(candidate, unit, name) {
                found = true;
                self.mark(candidate, Why::Via(from));
            }
        }
        if member {
            // A method is reached through a value, so its callers need not
            // import its file. Any same-language code downstream may call it.
            let origin = &ix.sources[file as usize];
            let reach = origin.project.map(|p| &ix.downstream[p]);
            for &candidate in ix.method_refs.get(name).into_iter().flatten() {
                let other = &ix.sources[candidate.0 as usize];
                let in_reach = match (reach, other.project) {
                    (Some(reach), Some(project)) => reach.contains(&project),
                    _ => true,
                };
                if from_test && !other.facts.is_test_file {
                    continue;
                }
                if candidate != from && other.facts.family == origin.facts.family && in_reach {
                    found = true;
                    self.mark(candidate, Why::Via(from));
                }
            }
        }
        found
    }
}
