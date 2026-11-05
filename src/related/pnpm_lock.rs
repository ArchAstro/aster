//! Attribute a `pnpm-lock.yaml` change to the workspace projects it affects.
//!
//! A pnpm v9 lockfile has three sections that name projects or packages:
//!
//! - `importers:` one entry per workspace project, keyed by its path relative
//!   to the lockfile, listing the versions the project resolved directly;
//! - `packages:` one entry per `name@version` (integrity, engines);
//! - `snapshots:` one entry per resolved `name@version(peers)` with the
//!   snapshot keys it depends on.
//!
//! Everything else (`lockfileVersion`, `settings`, `overrides`,
//! `patchedDependencies`, `catalogs`, checksums, ...) can change how every
//! project resolves, so a change there, or anything this parser does not
//! understand, selects every project ([`LockImpact::All`]). Otherwise the
//! impact is the importers whose own entry changed, plus the importers that
//! reach a changed package through the snapshot graph.
//!
//! There is no YAML dependency: the file is machine-written with a fixed
//! two-space layout, and the parser rejects any line outside that layout
//! instead of guessing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// What a lockfile edit reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockImpact {
    /// Cannot be attributed to projects; every project is affected.
    All,
    /// Importer paths (relative to the lockfile, `.` for the root) mapped to
    /// the reason each is affected. An importer absent here is unaffected.
    Importers(BTreeMap<String, String>),
}

#[derive(Debug, Default)]
struct Lock {
    /// Top-level scalars and blocks other than the three sections, as text.
    globals: BTreeMap<String, String>,
    importers: BTreeMap<String, String>,
    packages: BTreeMap<String, String>,
    snapshots: BTreeMap<String, String>,
}

/// Compare two lockfile texts.
pub fn lock_impact(old: &str, new: &str) -> LockImpact {
    let (Some(old), Some(new)) = (parse(old), parse(new)) else {
        return LockImpact::All;
    };
    if old.globals != new.globals {
        return LockImpact::All;
    }
    // Only the v9 layout is understood.
    let v9 = |lock: &Lock| {
        lock.globals
            .get("lockfileVersion")
            .is_some_and(|v| unquote(v).starts_with("9."))
    };
    if !v9(&old) || !v9(&new) {
        return LockImpact::All;
    }

    let mut out: BTreeMap<String, String> = BTreeMap::new();

    for path in changed_keys(&old.importers, &new.importers) {
        if new.importers.contains_key(&path) {
            let reason = format!("importer {path} changed");
            out.insert(path, reason);
        }
    }

    // Snapshot keys whose resolution changed: their own entry, or the
    // `packages:` entry of the same `name@version`.
    let mut changed: BTreeSet<String> = BTreeSet::new();
    changed.extend(changed_keys(&old.snapshots, &new.snapshots));
    for package in changed_keys(&old.packages, &new.packages) {
        changed.insert(package.clone());
        for key in old.snapshots.keys().chain(new.snapshots.keys()) {
            if base_key(key) == package {
                changed.insert(key.clone());
            }
        }
    }
    if changed.is_empty() {
        return LockImpact::Importers(out);
    }

    // Who depends on whom, over both sides so a removed edge still counts.
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for snapshots in [&old.snapshots, &new.snapshots] {
        for (key, body) in snapshots {
            for dep in snapshot_dependencies(body) {
                dependents.entry(dep).or_default().push(key.clone());
            }
        }
    }
    // Why each affected snapshot is affected: the changed package it reaches.
    let mut reached: HashMap<String, String> = HashMap::new();
    let mut queue: Vec<String> = Vec::new();
    for key in &changed {
        reached.insert(key.clone(), key.clone());
        queue.push(key.clone());
    }
    while let Some(key) = queue.pop() {
        let cause = reached[&key].clone();
        for parent in dependents.get(&key).into_iter().flatten() {
            if !reached.contains_key(parent) {
                reached.insert(parent.clone(), cause.clone());
                queue.push(parent.clone());
            }
        }
    }

    let snapshot_keys: HashSet<&String> =
        old.snapshots.keys().chain(new.snapshots.keys()).collect();
    for importers in [&old.importers, &new.importers] {
        for (path, body) in importers {
            if out.contains_key(path) {
                continue;
            }
            for (name, version) in importer_dependencies(body) {
                let hit = dependency_keys(&name, &version)
                    .into_iter()
                    .find(|key| snapshot_keys.contains(key) && reached.contains_key(key));
                if let Some(key) = hit {
                    out.insert(
                        path.clone(),
                        format!(
                            "importer {path} resolves {key}, which depends on changed {}",
                            reached[&key]
                        ),
                    );
                    break;
                }
            }
        }
    }
    // An importer that exists only in the old lockfile is gone.
    out.retain(|path, _| new.importers.contains_key(path));
    LockImpact::Importers(out)
}

fn changed_keys(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Vec<String> {
    old.keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| old.get(*key) != new.get(*key))
        .cloned()
        .collect()
}

/// `name@1.0.0(peer@2.0.0)` -> `name@1.0.0`.
fn base_key(key: &str) -> &str {
    match key.get(1..).and_then(|rest| rest.find('(')) {
        Some(i) => &key[..i + 1],
        None => key,
    }
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    for quote in ['\'', '"'] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
            return inner;
        }
    }
    text
}

/// Snapshot keys a dependency `name: version` can refer to: `name@version`,
/// or the version itself when it is an alias (`npm:` already stripped by
/// pnpm: `string-width-cjs: string-width@4.2.3`).
fn dependency_keys(name: &str, version: &str) -> Vec<String> {
    let mut keys = vec![format!("{name}@{version}")];
    if version.get(1..).is_some_and(|rest| rest.contains('@')) {
        keys.push(version.to_string());
    }
    keys
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// `name: value` where the name may be quoted. Returns (name, value).
fn split_key(text: &str) -> Option<(String, String)> {
    let text = text.trim_end();
    if let Some(quote) = text.chars().next().filter(|c| matches!(c, '\'' | '"')) {
        let close = text[1..].find(quote)? + 1;
        let rest = text[close + 1..].strip_prefix(':')?;
        return Some((text[1..close].to_string(), rest.trim().to_string()));
    }
    if let Some(key) = text.strip_suffix(':') {
        return Some((key.to_string(), String::new()));
    }
    let at = text.find(": ")?;
    Some((text[..at].to_string(), text[at + 2..].trim().to_string()))
}

fn parse(text: &str) -> Option<Lock> {
    let mut lock = Lock::default();
    // Current top-level key and, within a section, the current entry.
    let mut section: Option<String> = None;
    let mut entry: Option<(String, String)> = None;
    let mut global: Option<(String, String)> = None;

    fn flush_entry(
        lock: &mut Lock,
        section: &Option<String>,
        entry: &mut Option<(String, String)>,
    ) -> Option<()> {
        if let Some((key, body)) = entry.take() {
            let map = match section.as_deref()? {
                "importers" => &mut lock.importers,
                "packages" => &mut lock.packages,
                "snapshots" => &mut lock.snapshots,
                _ => return None,
            };
            map.insert(key, body).map_or(Some(()), |_| None)?; // duplicate key
        }
        Some(())
    }
    fn flush_global(lock: &mut Lock, global: &mut Option<(String, String)>) {
        if let Some((key, body)) = global.take() {
            lock.globals.insert(key, body);
        }
    }

    for line in text.lines() {
        if line.contains('\t') {
            return None;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = indent_of(line);
        if indent == 0 {
            flush_entry(&mut lock, &section, &mut entry)?;
            flush_global(&mut lock, &mut global);
            let (key, value) = split_key(line)?;
            if matches!(key.as_str(), "importers" | "packages" | "snapshots") {
                if !value.is_empty() && value != "{}" {
                    return None;
                }
                section = Some(key);
            } else {
                section = None;
                global = Some((key, value));
            }
            continue;
        }
        match &section {
            None => {
                let (_, body) = global.as_mut()?;
                body.push('\n');
                body.push_str(line);
            }
            Some(_) if indent == 2 => {
                flush_entry(&mut lock, &section, &mut entry)?;
                let (key, value) = split_key(line.trim_start())?;
                entry = Some((key, value));
            }
            Some(_) => {
                let (_, body) = entry.as_mut()?;
                body.push('\n');
                body.push_str(line);
            }
        }
    }
    flush_entry(&mut lock, &section, &mut entry)?;
    flush_global(&mut lock, &mut global);
    Some(lock)
}

/// `(name, version)` for each resolved direct dependency of an importer.
fn importer_dependencies(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    for line in body.lines() {
        let indent = indent_of(line);
        let text = line.trim();
        if indent == 6 {
            name = split_key(text).map(|(n, _)| n);
        } else if indent == 8 {
            if let (Some(n), Some(("version", v))) = (
                &name,
                split_key(text)
                    .as_ref()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            ) {
                out.push((n.clone(), unquote(v).to_string()));
            }
        }
    }
    out
}

/// Snapshot keys a snapshot depends on.
fn snapshot_dependencies(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for line in body.lines() {
        let indent = indent_of(line);
        let text = line.trim();
        if indent == 4 {
            in_deps = matches!(text, "dependencies:" | "optionalDependencies:");
        } else if indent == 6 && in_deps {
            if let Some((name, version)) = split_key(text) {
                out.extend(dependency_keys(&name, unquote(&version)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "\
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true

overrides:
  left-pad: 1.3.0

importers:

  .:
    devDependencies:
      typescript:
        specifier: ^5.0.0
        version: 5.9.3

  apps/a:
    dependencies:
      shared-lib:
        specifier: ^1.0.0
        version: 1.0.0
      only-a:
        specifier: ^2.0.0
        version: 2.0.0

  apps/b:
    dependencies:
      wrapper:
        specifier: ^3.0.0
        version: 3.0.0(react@19.0.0)

  infra:
    dependencies:
      infra-only:
        specifier: ^1.0.0
        version: 1.0.0

packages:

  infra-only@1.0.0:
    resolution: {integrity: sha512-aaa}

  only-a@2.0.0:
    resolution: {integrity: sha512-bbb}

  shared-lib@1.0.0:
    resolution: {integrity: sha512-ccc}

  typescript@5.9.3:
    resolution: {integrity: sha512-ddd}

  wrapper@3.0.0:
    resolution: {integrity: sha512-eee}

snapshots:

  infra-only@1.0.0: {}

  only-a@2.0.0: {}

  shared-lib@1.0.0: {}

  typescript@5.9.3: {}

  wrapper@3.0.0(react@19.0.0):
    dependencies:
      shared-lib: 1.0.0
";

    fn importers(impact: LockImpact) -> Vec<String> {
        match impact {
            LockImpact::Importers(map) => map.into_keys().collect(),
            LockImpact::All => panic!("expected importers, got All"),
        }
    }

    #[test]
    fn an_identical_lockfile_affects_nothing() {
        assert_eq!(importers(lock_impact(BASE, BASE)), Vec::<String>::new());
    }

    #[test]
    fn a_changed_importer_entry_affects_only_that_importer() {
        let new = BASE.replace("version: 1.0.0\n\npackages", "version: 1.0.1\n\npackages");
        assert_eq!(importers(lock_impact(BASE, &new)), vec!["infra"]);
    }

    #[test]
    fn a_changed_transitive_package_reaches_every_importer_that_depends_on_it() {
        // shared-lib is direct for apps/a and transitive through wrapper for apps/b.
        let new = BASE.replace("sha512-ccc", "sha512-zzz");
        assert_eq!(importers(lock_impact(BASE, &new)), vec!["apps/a", "apps/b"]);
    }

    #[test]
    fn a_changed_package_used_by_one_importer_leaves_the_others() {
        let new = BASE.replace("sha512-bbb", "sha512-yyy");
        assert_eq!(importers(lock_impact(BASE, &new)), vec!["apps/a"]);
    }

    #[test]
    fn a_removed_dependency_edge_still_reaches_the_importer() {
        let new = BASE.replace("    dependencies:\n      shared-lib: 1.0.0\n", "");
        assert_eq!(importers(lock_impact(BASE, &new)), vec!["apps/b"]);
    }

    #[test]
    fn an_aliased_dependency_is_followed_through_its_target_key() {
        let old = BASE.replace(
            "      shared-lib: 1.0.0\n",
            "      alias: shared-lib@1.0.0\n",
        );
        let new = old.replace("sha512-ccc", "sha512-zzz");
        assert_eq!(importers(lock_impact(&old, &new)), vec!["apps/a", "apps/b"]);
    }

    #[test]
    fn a_removed_importer_is_not_reported() {
        let new = BASE.replace("  infra:\n    dependencies:\n      infra-only:\n        specifier: ^1.0.0\n        version: 1.0.0\n\n", "");
        assert_eq!(importers(lock_impact(BASE, &new)), Vec::<String>::new());
    }

    #[test]
    fn a_global_change_selects_everything() {
        for (from, to) in [
            ("autoInstallPeers: true", "autoInstallPeers: false"),
            ("left-pad: 1.3.0", "left-pad: 1.3.1"),
            ("lockfileVersion: '9.0'", "lockfileVersion: '9.1'"),
        ] {
            assert_eq!(
                lock_impact(BASE, &BASE.replace(from, to)),
                LockImpact::All,
                "{from}"
            );
        }
        let patched = format!("patchedDependencies:\n  x@1:\n    hash: abc\n\n{BASE}");
        assert_eq!(lock_impact(BASE, &patched), LockImpact::All);
    }

    #[test]
    fn an_unparseable_or_unknown_layout_selects_everything() {
        assert_eq!(lock_impact(BASE, "importers:\n\tx: 1\n"), LockImpact::All);
        assert_eq!(lock_impact(BASE, ""), LockImpact::All);
        assert_eq!(lock_impact("", BASE), LockImpact::All);
        let v6 = "lockfileVersion: '6.0'\n\nimporters:\n";
        assert_eq!(
            lock_impact(v6, &format!("{v6}\n  .: {{}}\n")),
            LockImpact::All
        );
        // A line under a section that belongs to no entry.
        let orphan = BASE.replace("importers:\n", "importers:\n    stray: 1\n");
        assert_eq!(lock_impact(BASE, &orphan), LockImpact::All);
    }
}
