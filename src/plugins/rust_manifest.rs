//! Local Cargo manifest facts shared by discovery and related-test selection.
//! This reads paths and target declarations, not Cargo's package resolution.
use super::LocalDependency;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use toml::Value;

/// What kind of crate target a file is compiled into.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum LayoutKind {
    Lib,
    Bin(String),
    Test(String),
    Bench(String),
    Example(String),
    Build,
}

pub(crate) fn find_workspace_root(package_dir: &Path) -> Option<(PathBuf, Value)> {
    let mut dir = Some(package_dir);
    while let Some(current) = dir {
        if let Some(manifest) = std::fs::read_to_string(current.join("Cargo.toml"))
            .ok()
            .and_then(|s| toml::from_str::<Value>(&s).ok())
        {
            if current == package_dir {
                if let Some(root) = manifest
                    .get("package")
                    .and_then(|p| p.get("workspace"))
                    .and_then(Value::as_str)
                {
                    let root = normalize(&current.join(root));
                    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
                        .ok()
                        .and_then(|s| toml::from_str::<Value>(&s).ok())?;
                    return manifest.get("workspace").map(|_| (root, manifest.clone()));
                }
            }
            if manifest.get("workspace").is_some() {
                return Some((current.to_path_buf(), manifest));
            }
        }
        dir = current.parent();
    }
    None
}

/// Expand a `members` entry such as `crates/*` into directories.
pub(crate) fn expand_member_pattern(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for segment in Path::new(pattern).components() {
        let segment = segment.as_os_str().to_string_lossy().into_owned();
        let matcher = if segment.contains(['*', '?', '[']) {
            globset::Glob::new(&segment)
                .ok()
                .map(|g| g.compile_matcher())
        } else {
            None
        };
        let mut next = Vec::new();
        for dir in dirs {
            match &matcher {
                None => next.push(dir.join(&segment)),
                Some(matcher) => {
                    let Ok(entries) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        if entry.path().is_dir() && matcher.is_match(entry.file_name()) {
                            next.push(entry.path());
                        }
                    }
                }
            }
        }
        dirs = next;
    }
    dirs.into_iter().map(|d| normalize(&d)).collect()
}

/// Absolute directories of a manifest's path dependencies.
pub(crate) fn path_dependencies(
    manifest: &Value,
    dir: &Path,
    workspace_root: &Path,
    shared: Option<&Value>,
) -> Vec<LocalDependency> {
    let mut tables: Vec<&Value> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(key) {
            tables.push(table);
        }
    }
    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(table) = target.get(key) {
                    tables.push(table);
                }
            }
        }
    }
    let mut deps = Vec::new();
    for table in tables {
        let Some(table) = table.as_table() else {
            continue;
        };
        for (name, spec) in table {
            if let Some(path) = spec.get("path").and_then(Value::as_str) {
                deps.push(LocalDependency {
                    name: name.clone(),
                    path: normalize(&dir.join(path)),
                });
            } else if spec.get("workspace").and_then(Value::as_bool) == Some(true) {
                let shared_name = spec.get("package").and_then(Value::as_str).unwrap_or(name);
                if let Some(path) = shared
                    .and_then(|s| s.get(shared_name).or_else(|| s.get(name)))
                    .and_then(|s| s.get("path"))
                    .and_then(Value::as_str)
                {
                    deps.push(LocalDependency {
                        name: name.clone(),
                        path: normalize(&workspace_root.join(path)),
                    });
                }
            }
        }
    }
    deps
}

/// Declared and conventional target roots, without parsing any Rust source.
pub(crate) fn target_roots(manifest: &Value, dir: &Path) -> Vec<(LayoutKind, PathBuf)> {
    let package = manifest.get("package");
    let package_name = package
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auto = |key| {
        package
            .and_then(|p| p.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    let aux_kind = |kind, name| match kind {
        "tests" => LayoutKind::Test(name),
        "benches" => LayoutKind::Bench(name),
        _ => LayoutKind::Example(name),
    };
    let mut roots: Vec<(LayoutKind, PathBuf)> = Vec::new();
    let lib_root = manifest
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| {
            dir.join("src/lib.rs")
                .is_file()
                .then(|| PathBuf::from("src/lib.rs"))
        });
    if let Some(root) = lib_root {
        roots.push((LayoutKind::Lib, root));
    }
    let mut declared: HashSet<PathBuf> = HashSet::new();

    let tables = [
        ("bin", None),
        ("test", Some("tests")),
        ("bench", Some("benches")),
        ("example", Some("examples")),
    ];
    for (table, kind) in tables {
        let Some(entries) = manifest.get(table).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Cargo's inference for a target without `path`.
            let target_dir = match kind {
                None => "src/bin",
                Some(dir) => dir,
            };
            let mut candidates = Vec::new();
            if kind.is_none() && name == package_name {
                candidates.push(PathBuf::from("src/main.rs"));
            }
            candidates.push(PathBuf::from(format!("{target_dir}/{name}.rs")));
            candidates.push(PathBuf::from(format!("{target_dir}/{name}/main.rs")));
            let path = entry
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    candidates
                        .iter()
                        .find(|c| dir.join(c).is_file())
                        .cloned()
                        .unwrap_or_else(|| candidates[0].clone())
                });
            let owner = match kind {
                None => LayoutKind::Bin(name.to_string()),
                Some(kind) => aux_kind(kind, name.to_string()),
            };
            declared.insert(normalize(&path));
            roots.push((owner, path));
        }
    }

    let declared_names: HashSet<LayoutKind> = roots.iter().map(|(o, _)| o.clone()).collect();
    let add_auto = |owner: LayoutKind, path: PathBuf, roots: &mut Vec<(LayoutKind, PathBuf)>| {
        if !declared.contains(&normalize(&path)) && !declared_names.contains(&owner) {
            roots.push((owner, path));
        }
    };

    if auto("autobins") {
        if dir.join("src/main.rs").is_file() {
            add_auto(
                LayoutKind::Bin(package_name.to_string()),
                PathBuf::from("src/main.rs"),
                &mut roots,
            );
        }
        for (name, path) in discover_targets(dir, "src/bin") {
            add_auto(LayoutKind::Bin(name), path, &mut roots);
        }
    }
    for (key, target_dir, kind) in [
        ("autotests", "tests", "tests"),
        ("autobenches", "benches", "benches"),
        ("autoexamples", "examples", "examples"),
    ] {
        if auto(key) {
            for (name, path) in discover_targets(dir, target_dir) {
                add_auto(aux_kind(kind, name), path, &mut roots);
            }
        }
    }

    let build = manifest
        .get("package")
        .and_then(|p| p.get("build"))
        .and_then(|b| b.as_str().map(PathBuf::from))
        .or_else(|| {
            dir.join("build.rs")
                .is_file()
                .then(|| PathBuf::from("build.rs"))
        });
    if let Some(build) = build.filter(|_| {
        package
            .and_then(|p| p.get("build"))
            .and_then(Value::as_bool)
            != Some(false)
    }) {
        roots.push((LayoutKind::Build, build));
    }
    roots.sort();
    roots
}

/// Cargo's target auto-discovery in `dir`: `dir/*.rs` and `dir/*/main.rs`.
fn discover_targets(package_dir: &Path, dir: &str) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(package_dir.join(dir)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if path.is_file() {
            if let Some(stem) = file_name.strip_suffix(".rs") {
                found.push((stem.to_string(), PathBuf::from(dir).join(&file_name)));
            }
        } else if path.join("main.rs").is_file() {
            found.push((
                file_name.clone(),
                PathBuf::from(dir).join(&file_name).join("main.rs"),
            ));
        }
    }
    found.sort();
    found
}

/// Lexically normalize a path without requiring the input to exist.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some_and(|name| name != "..") {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
