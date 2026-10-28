//! Changes a target reads from outside its project directory.
//!
//! Ownership maps a changed file to the project whose directory holds it. A
//! target can also read workspace files outside that directory, declared as
//! `cache.workspace_include`. A change to one of those files changes the
//! target's cache key, so `aster affected` selects the target too.

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::discovery::DiscoveredProject;

/// Projects whose `target` declares a changed file among its workspace
/// inputs, with the matching files.
pub fn select_by_declared_inputs(
    target: &str,
    changed_files: &HashSet<PathBuf>,
    projects: &[DiscoveredProject],
) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut selected = BTreeMap::new();
    for project in projects {
        let Some(patterns) = project
            .targets
            .get(target)
            .and_then(|definition| definition.cache.as_ref())
            .map(|cache| &cache.workspace_include)
            .filter(|patterns| !patterns.is_empty())
        else {
            continue;
        };
        let address = format!("//{}", project.relative_path.display());
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(Glob::new(pattern).with_context(|| {
                format!("Invalid cache.workspace_include glob '{pattern}' in {address}:{target}")
            })?);
        }
        let globs = builder.build()?;
        let mut files: Vec<PathBuf> = changed_files
            .iter()
            .filter(|file| globs.is_match(file))
            .cloned()
            .collect();
        if !files.is_empty() {
            files.sort();
            selected.insert(address, files);
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CacheConfig;
    use crate::plugins::{ProjectMetadata, Target};
    use std::collections::HashMap;

    fn project(path: &str, target: &str, workspace_include: &[&str]) -> DiscoveredProject {
        let definition = Target {
            command: "true".into(),
            cache: Some(CacheConfig {
                workspace_include: workspace_include.iter().map(|s| s.to_string()).collect(),
                ..CacheConfig::default()
            }),
            ..Target::default()
        };
        DiscoveredProject {
            root: PathBuf::from("/ws").join(path),
            config_path: PathBuf::from("/ws").join(path).join("aster.toml"),
            metadata: ProjectMetadata {
                name: path.into(),
                version: None,
            },
            dependencies: vec![],
            targets: HashMap::from([(target.to_string(), definition)]),
            plugin_name: "generic".into(),
            languages: vec![],
            build_system: None,
            relative_path: PathBuf::from(path),
        }
    }

    fn changed(files: &[&str]) -> HashSet<PathBuf> {
        files.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_changed_workspace_input_selects_the_target_that_declares_it() {
        let projects = [
            project(
                "services/web",
                "browser-e2e",
                &["scripts/browser/**", ".mise.toml"],
            ),
            project("services/api", "browser-e2e", &["deps/**"]),
        ];
        let selected = select_by_declared_inputs(
            "browser-e2e",
            &changed(&["scripts/browser/run.sh", "README.md"]),
            &projects,
        )
        .unwrap();
        assert_eq!(
            selected,
            BTreeMap::from([(
                "//services/web".to_string(),
                vec![PathBuf::from("scripts/browser/run.sh")]
            )])
        );
    }

    #[test]
    fn only_the_requested_target_inputs_count() {
        let projects = [project("services/web", "browser-e2e", &[".mise.toml"])];
        let selected =
            select_by_declared_inputs("test", &changed(&[".mise.toml"]), &projects).unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn an_invalid_glob_names_the_target() {
        let projects = [project("services/web", "browser-e2e", &["scripts/[x"])];
        let error = select_by_declared_inputs("browser-e2e", &changed(&["a"]), &projects)
            .unwrap_err()
            .to_string();
        assert!(error.contains("//services/web:browser-e2e"), "{error}");
    }
}
