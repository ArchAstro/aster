//! File-to-project ownership mapping
//!
//! Maps changed files to the projects that own them and expands to include
//! transitive dependents.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;

/// Map changed files to the projects that own them.
///
/// A file belongs to the project with the longest `relative_path` that
/// contains it. A nested project therefore wins over its parent and over the
/// workspace root. A file that no project contains is ignored.
pub fn files_to_projects(
    changed_files: &HashSet<PathBuf>,
    projects: &[DiscoveredProject],
) -> HashSet<String> {
    let mut affected = HashSet::new();
    for file in changed_files {
        if let Some(owner) = most_specific_owner(projects, file) {
            affected.insert(format!("//{}", owner.relative_path.display()));
        }
    }
    affected
}

/// Changed files whose most specific owner is `project`.
///
/// Paths stay workspace-relative and keep the order of `changed_files`.
/// `projects` must be the full discovered set: a selected subset can omit a
/// nested owner (a lane or language filter) and would hand its files to a
/// shorter prefix, including the root's empty path.
pub fn owned_files<'a>(
    project: &DiscoveredProject,
    projects: &[DiscoveredProject],
    changed_files: impl IntoIterator<Item = &'a PathBuf>,
) -> Vec<&'a PathBuf> {
    changed_files
        .into_iter()
        .filter(|file| {
            most_specific_owner(projects, file)
                .is_some_and(|owner| owner.relative_path == project.relative_path)
        })
        .collect()
}

/// The project with the longest `relative_path` that contains `file`.
///
/// An empty `relative_path` is the workspace root and contains every file, so
/// it owns a file only when no nested project does.
fn most_specific_owner<'a>(
    projects: &'a [DiscoveredProject],
    file: &Path,
) -> Option<&'a DiscoveredProject> {
    projects
        .iter()
        .filter(|project| file.starts_with(&project.relative_path))
        .max_by_key(|project| project.relative_path.as_os_str().len())
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
            root: PathBuf::from(format!("/workspace/{relative_path}")),
            config_path: PathBuf::from(format!("/workspace/{relative_path}/package.json")),
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
            languages: vec!["nodejs".to_string()],
            build_system: None,
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
    fn test_files_to_projects_root_owns_only_files_outside_nested_projects() {
        let projects = vec![
            make_project("root", "", vec![]),
            make_project("pkg", "pkg", vec![]),
        ];
        let changed: HashSet<PathBuf> = vec![
            PathBuf::from("root.js"),
            PathBuf::from("pkg/x.js"),
            PathBuf::from("docs/guide.js"),
        ]
        .into_iter()
        .collect();

        let affected = files_to_projects(&changed, &projects);

        assert!(affected.contains("//"));
        assert!(affected.contains("//pkg"));
        assert_eq!(affected.len(), 2);
    }

    #[test]
    fn test_files_to_projects_nested_only_change_does_not_select_root() {
        let projects = vec![
            make_project("root", "", vec![]),
            make_project("pkg", "pkg", vec![]),
        ];
        let changed: HashSet<PathBuf> = vec![PathBuf::from("pkg/x.js")].into_iter().collect();

        let affected = files_to_projects(&changed, &projects);

        assert_eq!(affected, HashSet::from(["//pkg".to_string()]));
    }

    #[test]
    fn owned_files_gives_each_project_only_the_files_it_owns() {
        let projects = vec![
            make_project("root", "", vec![]),
            make_project("pkg", "pkg", vec![]),
            make_project("nested", "pkg/nested", vec![]),
            make_project("api", "services/api", vec![]),
            make_project("api-other", "services/api-other", vec![]),
        ];
        let changed = vec![
            PathBuf::from("root.js"),
            PathBuf::from("docs/guide.js"),
            PathBuf::from("pkg/x.js"),
            PathBuf::from("pkg/nested/y.js"),
            PathBuf::from("services/api/main.rs"),
            PathBuf::from("services/api-other/main.rs"),
        ];

        let names = |project: &DiscoveredProject| {
            owned_files(project, &projects, &changed)
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(names(&projects[0]), vec!["root.js", "docs/guide.js"]);
        assert_eq!(names(&projects[1]), vec!["pkg/x.js"]);
        assert_eq!(names(&projects[2]), vec!["pkg/nested/y.js"]);
        assert_eq!(names(&projects[3]), vec!["services/api/main.rs"]);
        assert_eq!(names(&projects[4]), vec!["services/api-other/main.rs"]);
    }

    #[test]
    fn owned_files_ignores_files_no_project_contains() {
        let projects = vec![make_project("pkg", "pkg", vec![])];
        let changed = vec![PathBuf::from("root.js")];

        assert!(owned_files(&projects[0], &projects, &changed).is_empty());
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
        let directly_affected: HashSet<String> =
            vec!["//libs/core".to_string(), "//services/web".to_string()]
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
