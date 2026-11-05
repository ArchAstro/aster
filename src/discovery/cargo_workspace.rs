//! Aster gives configured workspace actions one owner using its local project graph.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{DiscoveredProject, PartialProject};
use crate::config::project::CargoWorkspaceInputs;
use crate::config::TargetConfig;
use crate::plugins::rust_manifest::{
    expand_member_pattern, find_workspace_root, normalize, path_dependencies, target_roots,
};
use crate::plugins::Target;

pub(super) struct Owner {
    root: PathBuf,
    members: Vec<PathBuf>,
    targets: Vec<String>,
    target_directory: PathBuf,
    source_files: Vec<PathBuf>,
}

pub(super) fn prepare(
    projects: &mut [PartialProject],
    workspace_root: &Path,
) -> Result<Vec<Owner>> {
    let canonical_workspace = workspace_root.canonicalize()?;
    let mut owners = Vec::new();
    let mut local_dependencies = HashMap::new();
    for project in projects.iter() {
        let Some(config) = &project.cargo.workspace else {
            continue;
        };
        let address = format!("//{}", project.relative_path.display());
        if project.plugin_name != "rust" {
            bail!("{address}: [cargo.workspace] requires a Cargo workspace root");
        }
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(&project.config_path)?)?;
        if manifest.get("workspace").is_none() {
            bail!("{address}: [cargo.workspace] must be declared at the Cargo workspace root");
        }
        if config.targets.is_empty() {
            bail!("{address}: [cargo.workspace] targets must not be empty");
        }
        let mut seen = HashSet::new();
        for name in &config.targets {
            if !seen.insert(name) {
                bail!("{address}: duplicate Cargo workspace target '{name}'");
            }
            match project.custom_targets.get(name) {
                Some(TargetConfig::Simple(_) | TargetConfig::Rich(_)) => {}
                _ => bail!("{address}:{name}: Cargo workspace owner needs an explicit command in [targets.{name}]"),
            }
        }

        let cargo_root = project.root.canonicalize()?;
        let relative = |path: &Path| -> Result<PathBuf> {
            let canonical = path.canonicalize()?;
            canonical
                .strip_prefix(&canonical_workspace)
                .map(Path::to_path_buf)
                .with_context(|| {
                    format!(
                        "Cargo input {} is outside the Aster workspace",
                        path.display()
                    )
                })
        };
        let workspace = &manifest["workspace"];
        let paths = |key: &str| -> BTreeSet<PathBuf> {
            workspace
                .get(key)
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
                .flat_map(|pattern| expand_member_pattern(&cargo_root, pattern))
                .map(|path| path.canonicalize().unwrap_or(path))
                .collect()
        };
        // Exclusions are directory prefixes, not member glob expressions.
        let excluded: Vec<_> = workspace
            .get("exclude")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(|path| normalize(&cargo_root.join(path)))
            .map(|path| path.canonicalize().unwrap_or(path))
            .collect();
        let is_excluded = |path: &Path| excluded.iter().any(|root| path.starts_with(root));
        let mut pending = paths("members");
        pending.retain(|path| !is_excluded(path));
        if manifest.get("package").is_some() {
            pending.insert(cargo_root.clone());
        }
        let mut visited = HashSet::new();
        let mut members = Vec::new();
        let mut source_files = Vec::new();
        while let Some(package_root) = pending.pop_first() {
            if !visited.insert(package_root.clone()) {
                continue;
            }
            let member = relative(&package_root)?;
            let Some(discovered) = projects
                .iter()
                .find(|p| p.relative_path == member && p.plugin_name == "rust")
            else {
                bail!("{address}: Cargo member //{} was not discovered; include every owned member in the Aster checkout and discovery configuration", member.display());
            };
            let package: toml::Value =
                toml::from_str(&std::fs::read_to_string(&discovered.config_path)?)?;
            if package.get("package").is_none() {
                bail!(
                    "{address}: Cargo member //{} has no [package]",
                    member.display()
                );
            }
            let member_workspace =
                find_workspace_root(&package_root).and_then(|(root, _)| root.canonicalize().ok());
            if member_workspace.as_ref() != Some(&cargo_root) {
                bail!(
                    "{address}: Cargo member //{} belongs to a different workspace",
                    member.display()
                );
            }
            if discovered.root != project.root {
                for name in &config.targets {
                    if discovered.custom_targets.contains_key(name) {
                        bail!("//{}:{name} is owned by {address}:{name}; remove the member definition or use a different target name for a distinct operation", member.display());
                    }
                }
            }
            // Reuse the related-test manifest reader for the richer local graph
            // of configured owners, without changing unconfigured projects.
            let mut dependencies = path_dependencies(
                &package,
                &discovered.root,
                &project.root,
                workspace.get("dependencies"),
            );
            dependencies.retain(|dependency| {
                !dependency
                    .path
                    .canonicalize()
                    .is_ok_and(|path| path == package_root)
            });
            // Excluded/independent packages keep their graph edges, but do not
            // acquire forwarding targets from this owner.
            for dependency in &dependencies {
                if dependency.path.to_string_lossy().starts_with("//") {
                    continue;
                }
                let path = discovered.root.join(&dependency.path);
                let path = path.canonicalize().with_context(|| {
                    format!(
                        "Read local dependency {} of //{}",
                        path.display(),
                        member.display()
                    )
                })?;
                if is_excluded(&path) {
                    continue;
                }
                if find_workspace_root(&path)
                    .and_then(|(root, _)| root.canonicalize().ok())
                    .as_ref()
                    == Some(&cargo_root)
                {
                    pending.insert(path);
                }
            }
            for (_, source) in target_roots(&package, &package_root) {
                source_files.push(relative(&package_root.join(source))?);
            }
            local_dependencies.insert(member.clone(), dependencies);
            members.push(member);
        }
        members.sort();
        members.dedup();
        source_files.sort();
        source_files.dedup();
        let output = target_directory(&cargo_root)?;
        let target_directory = output
            .strip_prefix(&canonical_workspace)
            .map(Path::to_path_buf)
            .unwrap_or(output);
        owners.push(Owner {
            root: project.relative_path.clone(),
            members,
            targets: config.targets.clone(),
            target_directory,
            source_files,
        });
    }
    for project in projects.iter_mut() {
        if let Some(dependencies) = local_dependencies.remove(&project.relative_path) {
            project
                .dependencies
                .retain(|dependency| dependency.path.to_string_lossy().starts_with("//"));
            project.dependencies.extend(dependencies);
        }
    }
    Ok(owners)
}

/// Read only the output location needed to exclude build artifacts from inputs.
/// Cargo still interprets the build configuration when an operation executes.
fn target_directory(root: &Path) -> Result<PathBuf> {
    if let Some(path) =
        std::env::var_os("CARGO_TARGET_DIR").or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR"))
    {
        return Ok(normalize(&root.join(path)));
    }
    let mut configs: Vec<_> = root
        .ancestors()
        .map(|dir| (dir.join(".cargo"), dir.to_path_buf()))
        .collect();
    if let Some(home) = std::env::var_os("CARGO_HOME")
        .map(|home| normalize(&root.join(home)))
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
    {
        // Cargo resolves config-relative paths from the parent of .cargo.
        let base = home.parent().unwrap_or(&home).to_path_buf();
        configs.push((home, base));
    }
    for (dir, base) in configs {
        let legacy = dir.join("config");
        let config = if legacy.is_file() {
            legacy
        } else {
            dir.join("config.toml")
        };
        if !config.is_file() {
            continue;
        }
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&config)?)
            .with_context(|| format!("Read Cargo output directory from {}", config.display()))?;
        if let Some(path) = value
            .get("build")
            .and_then(|build| build.get("target-dir"))
            .and_then(toml::Value::as_str)
        {
            return Ok(normalize(&base.join(path)));
        }
    }
    Ok(root.join("target"))
}

pub(super) fn remove_inferred_builds(
    project: &PartialProject,
    owners: &[Owner],
    targets: &mut HashMap<String, Target>,
) {
    let Some(owner) = owners.iter().find(|owner| {
        owner.root == project.relative_path || owner.members.contains(&project.relative_path)
    }) else {
        return;
    };
    let member_builds: HashSet<_> = owner
        .members
        .iter()
        .map(|member| format!("//{}:build", member.display()))
        .collect();
    // Keep explicitly declared cross-project prerequisites. Only native Cargo
    // compilation edges are redundant; custom target overrides merge afterward.
    let explicit: HashSet<_> = project
        .dependencies
        .iter()
        .filter_map(|dep| dep.path.to_str().filter(|path| path.starts_with("//")))
        .map(|address| format!("{}:build", address.split(':').next().unwrap_or(address)))
        .collect();
    for target in targets.values_mut() {
        target
            .depends_on
            .retain(|dep| !member_builds.contains(dep) || explicit.contains(dep));
    }
}

pub(super) fn apply(owners: &[Owner], projects: &mut [DiscoveredProject]) -> Result<()> {
    for owner in owners {
        let mut excluded: Vec<_> = projects
            .iter()
            .map(|project| project.relative_path.clone())
            .collect();
        excluded.push(owner.target_directory.clone());
        for project in projects.iter_mut() {
            if project.relative_path == owner.root {
                for name in &owner.targets {
                    let target = project
                        .targets
                        .get_mut(name)
                        .context("Missing resolved Cargo owner")?;
                    let cache = target.cache.get_or_insert_with(Default::default);
                    let mut input_roots = owner.members.clone();
                    if name != "deps" {
                        for source in &owner.source_files {
                            if !input_roots.iter().any(|root| source.starts_with(root)) {
                                if let Some(parent) = source.parent() {
                                    input_roots.push(parent.to_path_buf());
                                }
                            }
                        }
                    }
                    cache.cargo_workspace = Some(CargoWorkspaceInputs {
                        roots: input_roots,
                        excluded: excluded.clone(),
                        manifests_only: name == "deps",
                    });
                    // A member target can live outside its package directory.
                    if name != "deps" {
                        cache.workspace_include.extend(
                            owner
                                .source_files
                                .iter()
                                .map(|p| p.to_string_lossy().replace('\\', "/")),
                        );
                    }
                    // A nested Cargo root uses its own lockfile and configuration.
                    for path in [
                        "Cargo.toml",
                        "Cargo.lock",
                        "aster.toml",
                        ".cargo/config.toml",
                        "rust-toolchain.toml",
                        "rust-toolchain",
                    ] {
                        cache
                            .workspace_include
                            .push(owner.root.join(path).to_string_lossy().replace('\\', "/"));
                    }
                }
            } else if owner.members.contains(&project.relative_path) {
                for name in &owner.targets {
                    project.targets.insert(
                        name.clone(),
                        Target {
                            depends_on: vec![format!("//{}:{name}", owner.root.display())],
                            ..Target::default()
                        },
                    );
                }
            }
        }
    }
    Ok(())
}
