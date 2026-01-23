//! File-to-project ownership mapping
//!
//! Maps changed files to the projects that own them and expands to include
//! transitive dependents.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;

/// Map changed files to the projects that own them
///
/// A project owns a file if the file path starts with the project's relative_path.
/// Projects are sorted by path length (longest first) to match most specific project
/// (e.g., services/api/submodule matches before services/api).
///
/// Files that don't belong to any project (e.g., workspace root files like README.md)
/// are ignored.
pub fn files_to_projects(
    changed_files: &HashSet<PathBuf>,
    projects: &[DiscoveredProject],
) -> HashSet<String> {
    // Sort projects by path length (longest first) to match most specific
    let mut sorted_projects: Vec<_> = projects.iter().collect();
    sorted_projects.sort_by(|a, b| {
        let len_a = a.relative_path.as_os_str().len();
        let len_b = b.relative_path.as_os_str().len();
        len_b.cmp(&len_a)
    });

    let mut affected = HashSet::new();

    for file in changed_files {
        // Find which project owns this file
        for project in &sorted_projects {
            if file.starts_with(&project.relative_path) {
                let address = format!("//{}", project.relative_path.display());
                affected.insert(address);
                break; // File belongs to most specific project
            }
        }
        // If no project owns the file, it's ignored (workspace root file)
    }

    affected
}

/// Expand directly affected projects to include all their dependents
///
/// Uses BFS to find all projects that transitively depend on the affected projects.
/// Returns the union of directly affected projects and all their dependents.
pub fn affected_with_dependents(
    directly_affected: HashSet<String>,
    graph: &ProjectGraph,
) -> HashSet<String> {
    let mut result = directly_affected.clone();
    let mut queue: VecDeque<String> = directly_affected.into_iter().collect();
    let mut visited: HashSet<String> = result.clone();

    while let Some(project) = queue.pop_front() {
        // Get all projects that depend ON this project
        let dependents = graph.dependents(&project);

        for dependent in dependents {
            if !visited.contains(&dependent) {
                visited.insert(dependent.clone());
                result.insert(dependent.clone());
                queue.push_back(dependent);
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::plugins::{LocalDependency, ProjectMetadata};
    use std::collections::HashMap;

    fn make_project(name: &str, relative_path: &str, deps: Vec<(&str, &str)>) -> DiscoveredProject {
        DiscoveredProject {
            root: PathBuf::from(format!("/workspace/{}", relative_path)),
            config_path: PathBuf::from(format!("/workspace/{}/package.json", relative_path)),
            metadata: ProjectMetadata {
                name: name.to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: deps
                .into_iter()
                .map(|(name, path)| LocalDependency {
                    name: name.to_string(),
                    path: PathBuf::from(path),
                })
                .collect(),
            targets: HashMap::new(),
            plugin_name: "nodejs".to_string(),
            relative_path: PathBuf::from(relative_path),
        }
    }

    #[test]
    fn test_files_to_projects_basic() {
        let projects = vec![
            make_project("api", "services/api", vec![]),
            make_project("core", "libs/core", vec![]),
        ];

        let changed: HashSet<PathBuf> = vec![
            PathBuf::from("services/api/src/main.rs"),
            PathBuf::from("services/api/package.json"),
        ]
        .into_iter()
        .collect();

        let affected = files_to_projects(&changed, &projects);

        assert_eq!(affected.len(), 1);
        assert!(affected.contains("//services/api"));
    }

    #[test]
    fn test_files_to_projects_multiple() {
        let projects = vec![
            make_project("api", "services/api", vec![]),
            make_project("core", "libs/core", vec![]),
        ];

        let changed: HashSet<PathBuf> = vec![
            PathBuf::from("services/api/src/main.rs"),
            PathBuf::from("libs/core/index.js"),
        ]
        .into_iter()
        .collect();

        let affected = files_to_projects(&changed, &projects);

        assert_eq!(affected.len(), 2);
        assert!(affected.contains("//services/api"));
        assert!(affected.contains("//libs/core"));
    }

    #[test]
    fn test_files_to_projects_ignores_workspace_files() {
        let projects = vec![make_project("api", "services/api", vec![])];

        let changed: HashSet<PathBuf> = vec![
            PathBuf::from("README.md"),
            PathBuf::from(".gitignore"),
            PathBuf::from("services/api/src/main.rs"),
        ]
        .into_iter()
        .collect();

        let affected = files_to_projects(&changed, &projects);

        assert_eq!(affected.len(), 1);
        assert!(affected.contains("//services/api"));
    }

    #[test]
    fn test_files_to_projects_most_specific_match() {
        // Nested projects: services/api/submodule is more specific than services/api
        let projects = vec![
            make_project("api", "services/api", vec![]),
            make_project("submodule", "services/api/submodule", vec![]),
        ];

        let changed: HashSet<PathBuf> = vec![PathBuf::from("services/api/submodule/file.rs")]
            .into_iter()
            .collect();

        let affected = files_to_projects(&changed, &projects);

        assert_eq!(affected.len(), 1);
        assert!(affected.contains("//services/api/submodule"));
        // Should NOT match services/api
        assert!(!affected.contains("//services/api"));
    }

    #[test]
    fn test_files_to_projects_empty_files() {
        let projects = vec![make_project("api", "services/api", vec![])];

        let changed: HashSet<PathBuf> = HashSet::new();

        let affected = files_to_projects(&changed, &projects);

        assert!(affected.is_empty());
    }

    #[test]
    fn test_affected_with_dependents_no_dependents() {
        let projects = vec![
            make_project("api", "services/api", vec![]),
            make_project("core", "libs/core", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        let directly_affected: HashSet<String> =
            vec!["//services/api".to_string()].into_iter().collect();

        let result = affected_with_dependents(directly_affected, &graph);

        // Only the directly affected project, no dependents
        assert_eq!(result.len(), 1);
        assert!(result.contains("//services/api"));
    }

    #[test]
    fn test_affected_with_dependents_single_dependent() {
        // api depends on core
        let projects = vec![
            make_project("api", "services/api", vec![("core", "//libs/core")]),
            make_project("core", "libs/core", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        // core is directly affected
        let directly_affected: HashSet<String> =
            vec!["//libs/core".to_string()].into_iter().collect();

        let result = affected_with_dependents(directly_affected, &graph);

        // core + api (which depends on core)
        assert_eq!(result.len(), 2);
        assert!(result.contains("//libs/core"));
        assert!(result.contains("//services/api"));
    }

    #[test]
    fn test_affected_with_dependents_transitive() {
        // c depends on b, b depends on a
        let projects = vec![
            make_project("c", "c", vec![("b", "//b")]),
            make_project("b", "b", vec![("a", "//a")]),
            make_project("a", "a", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        // a is directly affected
        let directly_affected: HashSet<String> = vec!["//a".to_string()].into_iter().collect();

        let result = affected_with_dependents(directly_affected, &graph);

        // a, b (depends on a), c (depends on b)
        assert_eq!(result.len(), 3);
        assert!(result.contains("//a"));
        assert!(result.contains("//b"));
        assert!(result.contains("//c"));
    }

    #[test]
    fn test_affected_with_dependents_diamond() {
        // d depends on both b and c, both depend on a
        //       a
        //      / \
        //     b   c
        //      \ /
        //       d
        let projects = vec![
            make_project("d", "d", vec![("b", "//b"), ("c", "//c")]),
            make_project("b", "b", vec![("a", "//a")]),
            make_project("c", "c", vec![("a", "//a")]),
            make_project("a", "a", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        // a is directly affected
        let directly_affected: HashSet<String> = vec!["//a".to_string()].into_iter().collect();

        let result = affected_with_dependents(directly_affected, &graph);

        // All 4 should be affected
        assert_eq!(result.len(), 4);
        assert!(result.contains("//a"));
        assert!(result.contains("//b"));
        assert!(result.contains("//c"));
        assert!(result.contains("//d"));
    }

    #[test]
    fn test_affected_with_dependents_multiple_affected() {
        // api depends on core, web is independent
        let projects = vec![
            make_project("api", "services/api", vec![("core", "//libs/core")]),
            make_project("web", "services/web", vec![]),
            make_project("core", "libs/core", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        // Both core and web are directly affected
        let directly_affected: HashSet<String> = vec![
            "//libs/core".to_string(),
            "//services/web".to_string(),
        ]
        .into_iter()
        .collect();

        let result = affected_with_dependents(directly_affected, &graph);

        // core, web, and api (which depends on core)
        assert_eq!(result.len(), 3);
        assert!(result.contains("//libs/core"));
        assert!(result.contains("//services/web"));
        assert!(result.contains("//services/api"));
    }
}
