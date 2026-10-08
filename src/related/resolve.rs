//! Turning the specifiers a definition uses (import paths, module names)
//! into the workspace files that provide them. Resolution works on the set
//! of known paths, so a file deleted by the change still resolves.

use super::facts::{Family, FileFacts};
use crate::plugins::rust_related::{self, LayoutKind};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

const JS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
/// Build output directories whose contents come from `src/`.
const JS_OUTPUT_DIRS: &[&str] = &["dist", "build", "lib", "out"];

/// What the resolvers know about the workspace.
pub struct Workspace<'a> {
    pub root: &'a Path,
    /// Every file, relative to the root.
    pub paths: &'a HashSet<PathBuf>,
    /// Analysed source files and their facts.
    pub sources: &'a HashMap<PathBuf, FileFacts>,
}

pub struct Resolvers {
    elixir_modules: HashMap<String, Vec<PathBuf>>,
    js_packages: HashMap<String, PathBuf>,
    js_configs: HashMap<PathBuf, TsConfig>,
    python_modules: HashMap<String, Vec<PathBuf>>,
    go_modules: Vec<(String, PathBuf)>,
    go_packages: HashMap<PathBuf, String>,
    pub rust: RustIndex,
}

/// One crate target and where its modules live.
pub struct RustTarget {
    pub kind: LayoutKind,
    /// Module path to file, relative to the workspace root.
    by_module: HashMap<Vec<String>, PathBuf>,
}

/// The crates of the workspace: which target and module each Rust file is,
/// so that a path can be followed to the file that declares what it names.
#[derive(Default)]
pub struct RustIndex {
    pub targets: Vec<RustTarget>,
    /// The targets a file is compiled into and its module path in each.
    files: HashMap<PathBuf, Vec<(usize, Vec<String>)>>,
    /// Library targets by crate name.
    libs: HashMap<String, usize>,
}

impl RustIndex {
    pub fn build<'p>(root: &Path, sources: impl Iterator<Item = &'p PathBuf>) -> Self {
        let mut index = RustIndex::default();
        // The package of a file is the nearest directory with a manifest.
        let mut packages: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for path in sources {
            let mut dir = path.parent();
            while let Some(candidate) = dir {
                if !seen.insert(candidate.to_path_buf()) {
                    break;
                }
                if root.join(candidate).join("Cargo.toml").is_file() {
                    packages.push(candidate.to_path_buf());
                }
                dir = candidate.parent();
            }
        }
        packages.sort();
        for dir in packages {
            let Some(layout) = rust_related::layout(&root.join(&dir)) else {
                continue;
            };
            for target in layout.targets {
                let id = index.targets.len();
                let mut by_module = HashMap::new();
                for (file, module) in target.files {
                    let file = normalize(&dir.join(file));
                    index
                        .files
                        .entry(file.clone())
                        .or_default()
                        .push((id, module.clone()));
                    by_module.entry(module).or_insert(file);
                }
                if target.kind == LayoutKind::Lib {
                    index.libs.insert(layout.lib_name.clone(), id);
                }
                index.targets.push(RustTarget {
                    kind: target.kind,
                    by_module,
                });
            }
        }
        index
    }

    /// The targets `file` is compiled into, with its module path in each.
    pub fn owners(&self, file: &Path) -> &[(usize, Vec<String>)] {
        self.files.get(file).map_or(&[], Vec::as_slice)
    }

    /// Follow `path` (`crate::a::b::Name`) from `from` as far as modules
    /// go: the file of the innermost module, and the segments left over,
    /// which name something inside it. A path that leads to no module of
    /// the workspace stays in `from`, whole.
    pub fn follow(&self, from: &Path, path: &str) -> Vec<(PathBuf, Vec<String>)> {
        let segments: Vec<String> = path.split("::").map(str::to_string).collect();
        let mut found: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for (target, module) in self.owners(from) {
            let mut target = *target;
            let mut base = module.clone();
            let mut rest = &segments[..];
            match rest.first().map(String::as_str) {
                Some("crate") => {
                    base.clear();
                    rest = &rest[1..];
                }
                Some("self") => rest = &rest[1..],
                Some("super") => {
                    while rest.first().is_some_and(|s| s == "super") {
                        if base.pop().is_none() {
                            break;
                        }
                        rest = &rest[1..];
                    }
                }
                Some(first) => {
                    let mut child = base.clone();
                    child.push(first.to_string());
                    if !self.targets[target].by_module.contains_key(&child) {
                        match self.libs.get(first) {
                            Some(lib) => {
                                target = *lib;
                                base.clear();
                                rest = &rest[1..];
                            }
                            None => continue,
                        }
                    }
                }
                None => continue,
            }
            let modules = &self.targets[target].by_module;
            let mut taken = 0;
            for segment in rest {
                base.push(segment.clone());
                if !modules.contains_key(&base) {
                    base.pop();
                    break;
                }
                taken += 1;
            }
            if let Some(file) = modules.get(&base) {
                let entry = (file.clone(), rest[taken..].to_vec());
                if !found.contains(&entry) {
                    found.push(entry);
                }
            }
        }
        if found.is_empty() {
            found.push((from.to_path_buf(), segments));
        }
        found
    }

    /// Files that hold only test code: integration tests, benches and
    /// examples, and the modules declared under `#[cfg(test)]`.
    pub fn test_files(&self, sources: &HashMap<PathBuf, FileFacts>) -> HashSet<PathBuf> {
        // Module paths declared `#[cfg(test)]`, by target.
        let mut under: HashMap<usize, Vec<Vec<String>>> = HashMap::new();
        for (file, owners) in &self.files {
            let Some(facts) = sources.get(file) else {
                continue;
            };
            for submodule in facts.submodules.iter().filter(|s| s.test) {
                for (target, module) in owners {
                    let mut path = module.clone();
                    path.extend(submodule.inline.iter().cloned());
                    path.push(submodule.name.clone());
                    under.entry(*target).or_default().push(path);
                }
            }
        }
        // A file is test code only if every target it is in says so.
        let is_test = |target: usize, module: &Vec<String>| match self.targets[target].kind {
            LayoutKind::Test(_) | LayoutKind::Bench(_) | LayoutKind::Example(_) => true,
            LayoutKind::Lib | LayoutKind::Bin(_) | LayoutKind::Build => under
                .get(&target)
                .is_some_and(|paths| paths.iter().any(|path| module.starts_with(path))),
        };
        self.files
            .iter()
            .filter(|(_, owners)| {
                owners
                    .iter()
                    .all(|(target, module)| is_test(*target, module))
            })
            .map(|(file, _)| file.clone())
            .collect()
    }
}

#[derive(Default, Clone)]
struct TsConfig {
    base_url: Option<PathBuf>,
    /// `(pattern, replacements)`, replacements already joined to their base.
    paths: Vec<(String, Vec<PathBuf>)>,
}

pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl Resolvers {
    pub fn build(ws: &Workspace) -> Self {
        let mut resolvers = Resolvers {
            elixir_modules: HashMap::new(),
            js_packages: HashMap::new(),
            js_configs: HashMap::new(),
            python_modules: HashMap::new(),
            go_modules: Vec::new(),
            go_packages: HashMap::new(),
            rust: RustIndex::build(
                ws.root,
                ws.sources
                    .iter()
                    .filter(|(_, facts)| facts.family == Family::Rust)
                    .map(|(path, _)| path),
            ),
        };
        for (path, facts) in ws.sources {
            match facts.family {
                Family::Elixir => {
                    for module in &facts.modules {
                        resolvers
                            .elixir_modules
                            .entry(module.clone())
                            .or_default()
                            .push(path.clone());
                    }
                }
                Family::Python => resolvers.index_python(ws, path),
                Family::Go => {
                    let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
                    if let Some(package) = facts.modules.first() {
                        let package = package.trim_end_matches("_test").to_string();
                        resolvers.go_packages.entry(dir).or_insert(package);
                    }
                }
                Family::Js | Family::Rust => {}
            }
        }
        for path in ws.paths {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
            match name {
                "package.json" => {
                    if let Some(package) = read_json(&ws.root.join(path))
                        .and_then(|json| json.get("name")?.as_str().map(str::to_string))
                    {
                        resolvers.js_packages.insert(package, dir);
                    }
                }
                "tsconfig.json" | "jsconfig.json" => {
                    let config = load_tsconfig(ws.root, path, 0);
                    resolvers.js_configs.entry(dir).or_insert(config);
                }
                "go.mod" => {
                    let module =
                        std::fs::read_to_string(ws.root.join(path))
                            .ok()
                            .and_then(|content| {
                                content.lines().find_map(|line| {
                                    line.trim()
                                        .strip_prefix("module ")
                                        .map(|m| m.trim().trim_matches('"').to_string())
                                })
                            });
                    if let Some(module) = module {
                        resolvers.go_modules.push((module, dir));
                    }
                }
                _ => {}
            }
        }
        // Longest module path first, so nested modules win.
        resolvers
            .go_modules
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.cmp(b)));
        resolvers
    }

    /// Whether `spec` reads as naming code in the workspace rather than a
    /// dependency from outside it: a relative path, a configured alias, a
    /// workspace package. One that then resolves to nothing is code the
    /// index cannot follow.
    pub fn internal(&self, from: &Path, family: Family, spec: &str) -> bool {
        match family {
            Family::Js => {
                if spec.starts_with('.') {
                    // An asset (`./logo.svg`) is not code.
                    let leaf = spec.rsplit('/').next().unwrap_or(spec);
                    let named = leaf
                        .rsplit_once('.')
                        .is_some_and(|(stem, _)| !stem.is_empty());
                    return !named || Family::of(Path::new(leaf)) == Some(Family::Js);
                }
                if spec.starts_with("@/") || spec.starts_with('~') || spec.starts_with('#') {
                    return true;
                }
                let mut current = from.parent();
                while let Some(dir) = current {
                    if let Some(config) = self.js_configs.get(dir) {
                        return config
                            .paths
                            .iter()
                            .any(|(pattern, _)| match_pattern(pattern, spec).is_some());
                    }
                    current = dir.parent();
                }
                false
            }
            Family::Python => {
                let first = spec.split('.').next().unwrap_or(spec);
                spec.starts_with('.') || self.python_modules.contains_key(first)
            }
            Family::Elixir | Family::Go | Family::Rust => false,
        }
    }

    /// The source files `spec` names when used from `from`.
    pub fn resolve(&self, ws: &Workspace, from: &Path, family: Family, spec: &str) -> Vec<PathBuf> {
        match family {
            Family::Elixir => self.elixir(ws, from, spec),
            Family::Js => self.js(ws, from, spec),
            Family::Python => self.python(ws, from, spec),
            Family::Go => self.go_dir(spec).into_iter().collect(),
            Family::Rust => self
                .rust
                .follow(from, spec)
                .into_iter()
                .map(|(file, _)| file)
                .collect(),
        }
    }

    // ------------------------------------------------------------------
    // Elixir
    // ------------------------------------------------------------------

    fn elixir(&self, ws: &Workspace, from: &Path, module: &str) -> Vec<PathBuf> {
        if let Some(files) = self.elixir_modules.get(module) {
            return files.clone();
        }
        // A module nested in this file is aliased inside its parent:
        // `Inner` for `Outer.Inner`. Nothing else resolves a partial name.
        let nested = ws.sources.get(from).is_some_and(|facts| {
            let suffix = format!(".{module}");
            facts
                .modules
                .iter()
                .any(|defined| defined.ends_with(&suffix))
        });
        if nested {
            return vec![from.to_path_buf()];
        }
        Vec::new()
    }

    // ------------------------------------------------------------------
    // JavaScript and TypeScript
    // ------------------------------------------------------------------

    fn js(&self, ws: &Workspace, from: &Path, spec: &str) -> Vec<PathBuf> {
        let dir = from.parent().unwrap_or(Path::new(""));
        if spec.starts_with('.') {
            return js_file(ws, &normalize(&dir.join(spec)))
                .into_iter()
                .collect();
        }
        // `paths` and `baseUrl` of the nearest tsconfig.
        let mut current = Some(dir);
        while let Some(candidate) = current {
            if let Some(config) = self.js_configs.get(candidate) {
                for (pattern, replacements) in &config.paths {
                    let Some(rest) = match_pattern(pattern, spec) else {
                        continue;
                    };
                    for replacement in replacements {
                        let target = replacement.to_string_lossy().replace('*', rest);
                        if let Some(found) = js_file(ws, &normalize(Path::new(&target))) {
                            return vec![found];
                        }
                    }
                }
                if let Some(found) = config
                    .base_url
                    .as_ref()
                    .and_then(|base| js_file(ws, &normalize(&base.join(spec))))
                {
                    return vec![found];
                }
                break;
            }
            current = candidate.parent();
        }
        // A workspace package.
        let mut parts = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
        let name = if spec.starts_with('@') {
            match (parts.next(), parts.next()) {
                (Some(scope), Some(name)) => format!("{scope}/{name}"),
                _ => return Vec::new(),
            }
        } else {
            parts.next().unwrap_or("").to_string()
        };
        let subpath = parts.next().unwrap_or("");
        let Some(package_dir) = self.js_packages.get(&name) else {
            return Vec::new();
        };
        if let Some(found) = js_package_entry(ws, package_dir, subpath) {
            return vec![found];
        }
        // The entry point is not in the tree (built output); depend on the
        // whole package.
        ws.sources
            .iter()
            .filter(|(path, facts)| facts.family == Family::Js && path.starts_with(package_dir))
            .map(|(path, _)| path.clone())
            .collect()
    }

    // ------------------------------------------------------------------
    // Python
    // ------------------------------------------------------------------

    fn index_python(&mut self, ws: &Workspace, path: &Path) {
        let mut parts: Vec<String> = path
            .parent()
            .into_iter()
            .flat_map(|p| p.components())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem != "__init__" {
            parts.push(stem.to_string());
        }
        let dir_count = path.parent().map(|p| p.components().count()).unwrap_or(0);
        for start in 0..parts.len() {
            // A module path starts at a directory that is not itself a
            // package.
            let root: PathBuf = parts[..start.min(dir_count)].iter().collect();
            if ws.paths.contains(&root.join("__init__.py")) {
                continue;
            }
            self.python_modules
                .entry(parts[start..].join("."))
                .or_default()
                .push(path.to_path_buf());
        }
    }

    fn python(&self, ws: &Workspace, from: &Path, module: &str) -> Vec<PathBuf> {
        let dots = module.chars().take_while(|c| *c == '.').count();
        if dots > 0 {
            let mut base = from.parent().unwrap_or(Path::new("")).to_path_buf();
            for _ in 1..dots {
                base.pop();
            }
            let rest = &module[dots..];
            let target = rest
                .split('.')
                .filter(|p| !p.is_empty())
                .fold(base, |p, s| p.join(s));
            return [target.with_extension("py"), target.join("__init__.py")]
                .into_iter()
                .filter(|candidate| ws.sources.contains_key(candidate))
                .collect();
        }
        let Some(candidates) = self.python_modules.get(module) else {
            return Vec::new();
        };
        // Prefer the candidates closest to the importing file.
        let shared = |candidate: &PathBuf| {
            candidate
                .components()
                .zip(from.components())
                .take_while(|(a, b)| a == b)
                .count()
        };
        let best = candidates.iter().map(shared).max().unwrap_or(0);
        candidates
            .iter()
            .filter(|c| shared(c) == best)
            .cloned()
            .collect()
    }

    /// `conftest.py` files that apply to `path` without being imported.
    pub fn python_implicit(&self, ws: &Workspace, path: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut dir = path.parent();
        while let Some(current) = dir {
            let conftest = current.join("conftest.py");
            if conftest != path && ws.sources.contains_key(&conftest) {
                found.push(conftest);
            }
            dir = current.parent();
        }
        found
    }

    // ------------------------------------------------------------------
    // Go
    // ------------------------------------------------------------------

    /// The workspace directory of an import path.
    pub fn go_dir(&self, import: &str) -> Option<PathBuf> {
        self.go_modules.iter().find_map(|(module, dir)| {
            if import == module {
                Some(dir.clone())
            } else {
                import
                    .strip_prefix(module.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .map(|rest| dir.join(rest))
            }
        })
    }

    /// The name code uses for an imported package.
    pub fn go_qualifier(&self, import: &str, alias: Option<&str>) -> String {
        if let Some(alias) = alias {
            return alias.to_string();
        }
        if let Some(package) = self
            .go_dir(import)
            .and_then(|dir| self.go_packages.get(&dir))
        {
            return package.clone();
        }
        let mut segments = import.rsplit('/');
        let last = segments.next().unwrap_or(import);
        let is_version = last.len() > 1
            && last.starts_with('v')
            && last[1..].chars().all(|c| c.is_ascii_digit());
        if is_version {
            segments.next().unwrap_or(last).to_string()
        } else {
            last.to_string()
        }
    }
}

/// The source file an extensionless module path names.
fn js_file(ws: &Workspace, path: &Path) -> Option<PathBuf> {
    if ws.sources.get(path).is_some_and(|f| f.family == Family::Js) {
        return Some(path.to_path_buf());
    }
    let name = path.file_name()?.to_str()?;
    for extension in JS_EXTENSIONS {
        let candidate = path.with_file_name(format!("{name}.{extension}"));
        if ws.sources.contains_key(&candidate) {
            return Some(candidate);
        }
    }
    // TypeScript sources imported by their output name: `./x.js` is `x.ts`.
    if let Some((stem, extension)) = name.rsplit_once('.') {
        let swaps: &[&str] = match extension {
            "js" => &["ts", "tsx", "jsx"],
            "jsx" => &["tsx"],
            "mjs" => &["mts"],
            "cjs" => &["cts"],
            _ => &[],
        };
        for swap in swaps {
            let candidate = path.with_file_name(format!("{stem}.{swap}"));
            if ws.sources.contains_key(&candidate) {
                return Some(candidate);
            }
        }
    }
    for extension in JS_EXTENSIONS {
        let candidate = path.join(format!("index.{extension}"));
        if ws.sources.contains_key(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The source behind a path that may point into build output.
fn js_source_for(ws: &Workspace, package_dir: &Path, target: &str) -> Option<PathBuf> {
    let target = target.trim_start_matches("./");
    if let Some(found) = js_file(ws, &normalize(&package_dir.join(target))) {
        return Some(found);
    }
    let (first, rest) = target.split_once('/')?;
    if !JS_OUTPUT_DIRS.contains(&first) {
        return None;
    }
    let rest = rest
        .trim_end_matches(".d.ts")
        .trim_end_matches(".d.mts")
        .trim_end_matches(".d.cts");
    [package_dir.join("src").join(rest), package_dir.join(rest)]
        .iter()
        .find_map(|candidate| js_file(ws, &normalize(candidate)))
}

fn js_package_entry(ws: &Workspace, package_dir: &Path, subpath: &str) -> Option<PathBuf> {
    let manifest = read_json(&ws.root.join(package_dir).join("package.json"));
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    if let Some(manifest) = &manifest {
        let mut targets = Vec::new();
        if let Some(exports) = manifest.get("exports") {
            match exports.get(&key) {
                Some(entry) => export_targets(entry, &mut targets),
                None if subpath.is_empty() => export_targets(exports, &mut targets),
                None => {}
            }
        }
        if subpath.is_empty() {
            for field in ["source", "module", "main", "types"] {
                if let Some(value) = manifest.get(field).and_then(|v| v.as_str()) {
                    targets.push(value.to_string());
                }
            }
        }
        if let Some(found) = targets
            .iter()
            .find_map(|target| js_source_for(ws, package_dir, target))
        {
            return Some(found);
        }
    }
    let fallbacks: Vec<PathBuf> = if subpath.is_empty() {
        vec![package_dir.join("src/index"), package_dir.join("index")]
    } else {
        vec![
            package_dir.join(subpath),
            package_dir.join("src").join(subpath),
        ]
    };
    fallbacks
        .iter()
        .find_map(|candidate| js_file(ws, &normalize(candidate)))
}

/// String targets of an `exports` entry, through condition objects.
fn export_targets(entry: &serde_json::Value, out: &mut Vec<String>) {
    match entry {
        serde_json::Value::String(target) => out.push(target.clone()),
        serde_json::Value::Object(conditions) => {
            for (key, value) in conditions {
                if !key.starts_with('.') {
                    export_targets(value, out);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|i| export_targets(i, out)),
        _ => {}
    }
}

/// The part of `spec` matched by the `*` of a tsconfig `paths` pattern.
fn match_pattern<'a>(pattern: &str, spec: &'a str) -> Option<&'a str> {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => spec.strip_prefix(prefix)?.strip_suffix(suffix),
        None => (pattern == spec).then_some(""),
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content)
        .ok()
        .or_else(|| serde_json::from_str(&strip_jsonc(&content)).ok())
}

/// Remove comments and trailing commas, which tsconfig files allow.
fn strip_jsonc(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn load_tsconfig(root: &Path, path: &Path, depth: usize) -> TsConfig {
    let dir = path.parent().unwrap_or(Path::new(""));
    let Some(json) = read_json(&root.join(path)) else {
        return TsConfig::default();
    };
    let mut config = match json.get("extends").and_then(|e| e.as_str()) {
        Some(parent) if parent.starts_with('.') && depth < 5 => {
            let mut parent_path = normalize(&dir.join(parent));
            if parent_path.extension().is_none() {
                parent_path.set_extension("json");
            }
            load_tsconfig(root, &parent_path, depth + 1)
        }
        _ => TsConfig::default(),
    };
    let Some(options) = json.get("compilerOptions") else {
        return config;
    };
    let base = options
        .get("baseUrl")
        .and_then(|b| b.as_str())
        .map(|b| normalize(&dir.join(b)));
    if let Some(paths) = options.get("paths").and_then(|p| p.as_object()) {
        let paths_base = base.clone().unwrap_or_else(|| dir.to_path_buf());
        config.paths = paths
            .iter()
            .map(|(pattern, replacements)| {
                let replacements = replacements
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_str())
                    .map(|r| paths_base.join(r))
                    .collect();
                (pattern.clone(), replacements)
            })
            .collect();
    }
    if base.is_some() {
        config.base_url = base;
    }
    config
}
