//! Cargo resolves membership; Aster gives each declared action one graph owner.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{DiscoveredProject, PartialProject};
use crate::config::project::CargoWorkspaceInputs;
use crate::config::TargetConfig;
use crate::plugins::{LocalDependency, Target};

pub(super) struct Owner {
    root: PathBuf,
    members: Vec<PathBuf>,
    targets: Vec<String>,
    target_directory: PathBuf,
    source_files: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    workspace_members: Vec<String>,
    target_directory: PathBuf,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    manifest_path: PathBuf,
    targets: Vec<CargoTarget>,
    dependencies: Vec<CargoDependency>,
}

#[derive(Deserialize)]
struct CargoDependency {
    name: String,
    rename: Option<String>,
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CargoTarget {
    src_path: PathBuf,
}

pub(super) fn prepare(
    projects: &mut [PartialProject],
    workspace_root: &Path,
) -> Result<Vec<Owner>> {
    let canonical_workspace = workspace_root.canonicalize()?;
    let mut owners = Vec::new();
    let mut cargo_dependencies = HashMap::new();
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

        // No dependency resolution, downloads, or lockfile updates during discovery.
        // Cargo includes globbed, implicit path, and package.workspace members.
        let output = Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--locked",
                "--format-version",
                "1",
            ])
            .current_dir(&project.root)
            .output()
            .with_context(|| format!("Discover Cargo workspace ownership for {address}"))?;
        if !output.status.success() {
            bail!(
                "Cargo workspace metadata failed for {address}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let metadata: Metadata = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("Read Cargo workspace metadata for {address}"))?;
        if metadata.workspace_root.canonicalize()? != project.root.canonicalize()? {
            bail!(
                "{address}: [cargo.workspace] must be declared at the Cargo workspace root {}",
                metadata.workspace_root.display()
            );
        }
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
        let ids: HashSet<_> = metadata.workspace_members.iter().collect();
        let mut members = Vec::new();
        let mut source_files = Vec::new();
        for package in &metadata.packages {
            if !ids.contains(&package.id) {
                continue;
            }
            let member = relative(
                package
                    .manifest_path
                    .parent()
                    .context("Cargo manifest has no parent")?,
            )?;
            let Some(discovered) = projects
                .iter()
                .find(|p| p.relative_path == member && p.plugin_name == "rust")
            else {
                bail!("{address}: Cargo member //{} was not discovered; include every owned member in the Aster checkout and discovery configuration", member.display());
            };
            let package_root = discovered.root.canonicalize()?;
            cargo_dependencies.insert(
                member.clone(),
                package
                    .dependencies
                    .iter()
                    .filter_map(|dependency| {
                        let path = dependency.path.as_ref()?;
                        // Cargo permits a renamed self dev-dependency (often
                        // to enable test-only features). It is not an edge to
                        // another Aster project and must not form a self cycle.
                        if path.canonicalize().is_ok_and(|path| path == package_root) {
                            return None;
                        }
                        Some(LocalDependency {
                            name: dependency
                                .rename
                                .as_ref()
                                .unwrap_or(&dependency.name)
                                .clone(),
                            path: path.clone(),
                        })
                    })
                    .collect::<Vec<_>>(),
            );
            if discovered.root != project.root {
                for name in &config.targets {
                    if discovered.custom_targets.contains_key(name) {
                        bail!("//{}:{name} is owned by {address}:{name}; remove the member definition or use a different target name for a distinct operation", member.display());
                    }
                }
            }
            for target in &package.targets {
                source_files.push(relative(&target.src_path)?);
            }
            members.push(member);
        }
        members.sort();
        members.dedup();
        source_files.sort();
        source_files.dedup();
        // The output directory may not exist yet. Metadata returns an absolute path.
        let target_directory = metadata
            .target_directory
            .strip_prefix(&metadata.workspace_root)
            .map(|path| project.relative_path.join(path))
            .or_else(|_| {
                metadata
                    .target_directory
                    .strip_prefix(&canonical_workspace)
                    .map(Path::to_path_buf)
            })
            .unwrap_or(metadata.target_directory);
        owners.push(Owner {
            root: project.relative_path.clone(),
            members,
            targets: config.targets.clone(),
            target_directory,
            source_files,
        });
    }
    for project in projects.iter_mut() {
        if let Some(native) = cargo_dependencies.remove(&project.relative_path) {
            // Metadata also resolves workspace-inherited and target-specific
            // path dependencies. Preserve that source graph for affected/cache
            // selection even though Cargo owns the compilation ordering.
            project
                .dependencies
                .retain(|dependency| dependency.path.to_string_lossy().starts_with("//"));
            project.dependencies.extend(native);
        }
    }
    Ok(owners)
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
