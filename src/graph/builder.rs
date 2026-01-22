//! Graph construction from discovered projects
//!
//! Builds a petgraph DiGraph where nodes are projects and edges represent
//! dependencies between them.

use std::collections::HashMap;
use std::path::PathBuf;

use petgraph::graph::{DiGraph, NodeIndex};

use crate::discovery::DiscoveredProject;

/// A node in the dependency graph representing a project
#[derive(Debug, Clone)]
pub struct ProjectNode {
    /// Project address (//path/to/project)
    pub address: String,
    /// Project name (from metadata)
    pub name: String,
    /// Absolute path to project root
    pub root: PathBuf,
}

/// The dependency graph of projects
pub struct ProjectGraph {
    /// The underlying directed graph
    pub graph: DiGraph<ProjectNode, ()>,
    /// Map from address to node index for fast lookups
    pub index_by_address: HashMap<String, NodeIndex>,
}

/// Build a dependency graph from discovered projects
///
/// Algorithm:
/// 1. Add all projects as nodes
/// 2. Add edges for dependencies (dependent -> dependency)
/// 3. Warn about missing dependencies (may be cross-language)
pub fn build_graph(projects: &[DiscoveredProject]) -> anyhow::Result<ProjectGraph> {
    let mut graph = DiGraph::new();
    let mut index_by_address = HashMap::new();

    // First pass: Add all projects as nodes
    for project in projects {
        let address = format!("//{}", project.relative_path.display());
        let node = ProjectNode {
            address: address.clone(),
            name: project.metadata.name.clone(),
            root: project.root.clone(),
        };

        let idx = graph.add_node(node);
        index_by_address.insert(address, idx);
    }

    // Second pass: Add edges for dependencies
    for project in projects {
        let from_address = format!("//{}", project.relative_path.display());
        let from_idx = index_by_address[&from_address];

        for dep in &project.dependencies {
            // Resolve dependency to an address
            let dep_address = resolve_dependency_address(project, dep);

            if let Some(&to_idx) = index_by_address.get(&dep_address) {
                // Add edge: dependent -> dependency
                graph.add_edge(from_idx, to_idx, ());
            } else {
                // Dependency not found - warn but continue
                // This can happen with cross-language dependencies
                eprintln!(
                    "warning: dependency {} from {} not found in workspace",
                    dep_address, from_address
                );
            }
        }
    }

    Ok(ProjectGraph {
        graph,
        index_by_address,
    })
}

/// Resolve a dependency to its address
///
/// Dependencies can be:
/// - Already an address: //libs/shared or //libs/shared:build
/// - A relative file path: ../../libs/shared
fn resolve_dependency_address(project: &DiscoveredProject, dep: &crate::plugins::LocalDependency) -> String {
    let path_str = dep.path.to_string_lossy();

    // If it already looks like an address, use it (strip any target suffix)
    if path_str.starts_with("//") {
        // Strip target suffix if present (e.g., //libs/shared:build -> //libs/shared)
        if let Some(colon_pos) = path_str.find(':') {
            return path_str[..colon_pos].to_string();
        }
        return path_str.to_string();
    }

    // Otherwise, resolve relative path from project's directory
    let project_dir = &project.relative_path;
    let resolved = project_dir.join(&dep.path);

    // Normalize the path (handle .. components)
    let normalized = normalize_path(&resolved);

    format!("//{}", normalized.display())
}

/// Normalize a path by resolving .. components
fn normalize_path(path: &std::path::Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::Normal(c) => {
                components.push(c);
            }
            std::path::Component::CurDir => {}
            _ => {}
        }
    }

    components.iter().collect()
}

impl ProjectGraph {
    /// Return projects in dependency order (dependencies before dependents)
    ///
    /// Panics if graph has cycles - call find_cycle first
    pub fn topological_order(&self) -> Vec<&ProjectNode> {
        use petgraph::algo::toposort;

        let sorted = toposort(&self.graph, None).expect("Graph has cycles - should have been checked");

        sorted.iter().map(|idx| &self.graph[*idx]).collect()
    }

    /// Get a project node by address
    pub fn get(&self, address: &str) -> Option<&ProjectNode> {
        self.index_by_address.get(address).map(|idx| &self.graph[*idx])
    }

    /// Get all project nodes
    pub fn projects(&self) -> impl Iterator<Item = &ProjectNode> {
        self.graph.node_weights()
    }

    /// Get direct dependencies of a project
    pub fn dependencies(&self, address: &str) -> Vec<&ProjectNode> {
        if let Some(&idx) = self.index_by_address.get(address) {
            self.graph
                .neighbors(idx)
                .map(|dep_idx| &self.graph[dep_idx])
                .collect()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_build_empty_graph() {
        let projects: Vec<DiscoveredProject> = vec![];
        let graph = build_graph(&projects).unwrap();

        assert_eq!(graph.graph.node_count(), 0);
        assert_eq!(graph.graph.edge_count(), 0);
    }

    #[test]
    fn test_build_single_project() {
        let projects = vec![make_project("api", "services/api", vec![])];

        let graph = build_graph(&projects).unwrap();

        assert_eq!(graph.graph.node_count(), 1);
        assert_eq!(graph.graph.edge_count(), 0);

        let node = graph.get("//services/api").unwrap();
        assert_eq!(node.name, "api");
        assert_eq!(node.address, "//services/api");
    }

    #[test]
    fn test_build_with_dependencies() {
        let projects = vec![
            make_project("core", "libs/core", vec![]),
            make_project("api", "services/api", vec![("core", "//libs/core")]),
        ];

        let graph = build_graph(&projects).unwrap();

        assert_eq!(graph.graph.node_count(), 2);
        assert_eq!(graph.graph.edge_count(), 1);

        // api depends on core
        let api_deps = graph.dependencies("//services/api");
        assert_eq!(api_deps.len(), 1);
        assert_eq!(api_deps[0].address, "//libs/core");
    }

    #[test]
    fn test_build_with_relative_path_dependencies() {
        let projects = vec![
            make_project("core", "libs/core", vec![]),
            make_project("api", "services/api", vec![("core", "../../libs/core")]),
        ];

        let graph = build_graph(&projects).unwrap();

        assert_eq!(graph.graph.node_count(), 2);
        assert_eq!(graph.graph.edge_count(), 1);

        let api_deps = graph.dependencies("//services/api");
        assert_eq!(api_deps.len(), 1);
        assert_eq!(api_deps[0].address, "//libs/core");
    }

    #[test]
    fn test_topological_order() {
        // C depends on B, B depends on A
        // Expected order: A, B, C (dependencies first)
        let projects = vec![
            make_project("c", "c", vec![("b", "//b")]),
            make_project("b", "b", vec![("a", "//a")]),
            make_project("a", "a", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();
        let order = graph.topological_order();

        // Find positions
        let pos_a = order.iter().position(|n| n.address == "//a").unwrap();
        let pos_b = order.iter().position(|n| n.address == "//b").unwrap();
        let pos_c = order.iter().position(|n| n.address == "//c").unwrap();

        // A must come before B, B must come before C
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn test_get_project() {
        let projects = vec![
            make_project("api", "services/api", vec![]),
            make_project("web", "services/web", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        assert!(graph.get("//services/api").is_some());
        assert!(graph.get("//services/web").is_some());
        assert!(graph.get("//nonexistent").is_none());
    }

    #[test]
    fn test_dependencies_method() {
        // A depends on B and C, B depends on C
        let projects = vec![
            make_project("a", "a", vec![("b", "//b"), ("c", "//c")]),
            make_project("b", "b", vec![("c", "//c")]),
            make_project("c", "c", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();

        // A has two direct deps
        let a_deps = graph.dependencies("//a");
        assert_eq!(a_deps.len(), 2);

        // B has one direct dep
        let b_deps = graph.dependencies("//b");
        assert_eq!(b_deps.len(), 1);
        assert_eq!(b_deps[0].address, "//c");

        // C has no deps
        let c_deps = graph.dependencies("//c");
        assert!(c_deps.is_empty());
    }

    #[test]
    fn test_projects_iterator() {
        let projects = vec![
            make_project("a", "a", vec![]),
            make_project("b", "b", vec![]),
            make_project("c", "c", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();
        let all_projects: Vec<_> = graph.projects().collect();

        assert_eq!(all_projects.len(), 3);
    }

    #[test]
    fn test_address_with_target_suffix_stripped() {
        // Dependency with target suffix should be resolved to base address
        let projects = vec![
            make_project("core", "libs/core", vec![]),
            make_project("api", "services/api", vec![("core", "//libs/core:build")]),
        ];

        let graph = build_graph(&projects).unwrap();

        // Edge should be created to //libs/core (not //libs/core:build)
        let api_deps = graph.dependencies("//services/api");
        assert_eq!(api_deps.len(), 1);
        assert_eq!(api_deps[0].address, "//libs/core");
    }
}
