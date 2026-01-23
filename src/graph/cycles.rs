//! Cycle detection for dependency graphs
//!
//! Detects cycles and returns the exact cycle path for clear error reporting.

use std::collections::HashSet;

use super::ProjectGraph;

/// Error returned when a dependency cycle is detected
#[derive(Debug, Clone)]
pub struct CycleError {
    /// The cycle path: ["//a", "//b", "//c", "//a"]
    pub path: Vec<String>,
}

impl std::fmt::Display for CycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dependency cycle detected: {}", self.path.join(" -> "))
    }
}

impl std::error::Error for CycleError {}

/// Detect cycles in the project graph and return the exact path if found
///
/// Uses DFS with a recursion stack to detect back-edges. When a cycle is found,
/// extracts the exact cycle path from the recursion stack.
pub fn find_cycle(graph: &ProjectGraph) -> Option<CycleError> {
    let mut visited = HashSet::new();
    let mut rec_stack = Vec::new();
    let mut rec_set = HashSet::new();

    // Try starting DFS from each unvisited node
    for node_idx in graph.graph.node_indices() {
        if !visited.contains(&node_idx) {
            if let Some(cycle) =
                dfs_cycle(graph, node_idx, &mut visited, &mut rec_stack, &mut rec_set)
            {
                return Some(cycle);
            }
        }
    }

    None
}

/// DFS helper that tracks the recursion path for cycle extraction
fn dfs_cycle(
    graph: &ProjectGraph,
    node: petgraph::graph::NodeIndex,
    visited: &mut HashSet<petgraph::graph::NodeIndex>,
    rec_stack: &mut Vec<petgraph::graph::NodeIndex>,
    rec_set: &mut HashSet<petgraph::graph::NodeIndex>,
) -> Option<CycleError> {
    visited.insert(node);
    rec_stack.push(node);
    rec_set.insert(node);

    // Check all neighbors (dependencies)
    for neighbor in graph.graph.neighbors(node) {
        if !visited.contains(&neighbor) {
            // Not yet visited - recurse
            if let Some(cycle) = dfs_cycle(graph, neighbor, visited, rec_stack, rec_set) {
                return Some(cycle);
            }
        } else if rec_set.contains(&neighbor) {
            // Back-edge found - extract cycle from recursion stack
            let cycle_start_addr = &graph.graph[neighbor].address;

            // Find where the cycle starts in rec_stack
            let cycle_start_pos = rec_stack
                .iter()
                .position(|&idx| idx == neighbor)
                .expect("Cycle start should be in recursion stack");

            // Extract the cycle path
            let mut path: Vec<String> = rec_stack[cycle_start_pos..]
                .iter()
                .map(|&idx| graph.graph[idx].address.clone())
                .collect();

            // Add the first node again to complete the cycle
            path.push(cycle_start_addr.clone());

            return Some(CycleError { path });
        }
    }

    // Backtrack
    rec_stack.pop();
    rec_set.remove(&node);

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoveredProject;
    use crate::graph::build_graph;
    use crate::plugins::{LocalDependency, ProjectMetadata};
    use std::collections::HashMap;
    use std::path::PathBuf;

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
            relative_path: PathBuf::from(relative_path),
        }
    }

    #[test]
    fn test_no_cycle() {
        // A -> B -> C (no cycle)
        let projects = vec![
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![("c", "//c")]),
            make_project("c", "c", vec![]),
        ];

        let graph = build_graph(&projects).unwrap();
        assert!(find_cycle(&graph).is_none());
    }

    #[test]
    fn test_simple_cycle() {
        // A -> B -> A (cycle)
        let projects = vec![
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![("a", "//a")]),
        ];

        let graph = build_graph(&projects).unwrap();
        let cycle = find_cycle(&graph);
        assert!(cycle.is_some());

        let cycle = cycle.unwrap();
        // Cycle should be ["//a", "//b", "//a"] or ["//b", "//a", "//b"]
        assert!(cycle.path.len() == 3);
        assert_eq!(cycle.path.first(), cycle.path.last());
    }

    #[test]
    fn test_longer_cycle() {
        // A -> B -> C -> A (3-node cycle)
        let projects = vec![
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![("c", "//c")]),
            make_project("c", "c", vec![("a", "//a")]),
        ];

        let graph = build_graph(&projects).unwrap();
        let cycle = find_cycle(&graph);
        assert!(cycle.is_some());

        let cycle = cycle.unwrap();
        assert!(cycle.path.len() == 4); // A -> B -> C -> A
        assert_eq!(cycle.path.first(), cycle.path.last());
    }

    #[test]
    fn test_cycle_in_subgraph() {
        // D -> A -> B -> A (cycle in subgraph, D not involved)
        let projects = vec![
            make_project("d", "d", vec![("a", "//a")]),
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![("a", "//a")]),
        ];

        let graph = build_graph(&projects).unwrap();
        let cycle = find_cycle(&graph);
        assert!(cycle.is_some());

        let cycle = cycle.unwrap();
        // Cycle should only include A and B, not D
        assert!(!cycle.path.contains(&"//d".to_string()));
    }

    #[test]
    fn test_cycle_error_display() {
        let err = CycleError {
            path: vec![
                "//a".to_string(),
                "//b".to_string(),
                "//c".to_string(),
                "//a".to_string(),
            ],
        };
        assert_eq!(
            err.to_string(),
            "Dependency cycle detected: //a -> //b -> //c -> //a"
        );
    }
}
