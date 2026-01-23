//! Path finding between projects in the dependency graph
//!
//! Uses A* algorithm to find the shortest dependency path between two projects.

use petgraph::algo::astar;

use crate::graph::ProjectGraph;

/// Find the shortest dependency path between two projects
///
/// Returns None if no path exists or if either project is not found.
/// Returns Some(path) where path is a vector of project addresses from `from` to `to`.
pub fn find_path(graph: &ProjectGraph, from: &str, to: &str) -> Option<Vec<String>> {
    let from_idx = graph.index_by_address.get(from)?;
    let to_idx = graph.index_by_address.get(to)?;

    // Use A* with uniform edge cost (1)
    let result = astar(
        &graph.graph,
        *from_idx,
        |node| node == *to_idx,
        |_| 1, // Uniform edge cost
        |_| 0, // No heuristic (Dijkstra behavior)
    );

    result.map(|(_, path)| {
        path.iter()
            .map(|idx| graph.graph[*idx].address.clone())
            .collect()
    })
}

/// Format a dependency path as a human-readable string
///
/// Example: ["//a", "//b", "//c"] -> "//a -> //b -> //c"
pub fn format_path(path: &[String]) -> String {
    path.join(" -> ")
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
    fn test_find_path_direct() {
        // A depends on B (direct path)
        let projects = vec![
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![]),
        ];
        let graph = build_graph(&projects).unwrap();

        let path = find_path(&graph, "//a", "//b");
        assert!(path.is_some());
        let path = path.unwrap();
        assert_eq!(path, vec!["//a", "//b"]);
    }

    #[test]
    fn test_find_path_transitive() {
        // A -> B -> C (transitive path)
        let projects = vec![
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![("c", "//c")]),
            make_project("c", "c", vec![]),
        ];
        let graph = build_graph(&projects).unwrap();

        let path = find_path(&graph, "//a", "//c");
        assert!(path.is_some());
        let path = path.unwrap();
        assert_eq!(path, vec!["//a", "//b", "//c"]);
    }

    #[test]
    fn test_find_path_no_path() {
        // A and B are unconnected
        let projects = vec![
            make_project("a", "a", vec![]),
            make_project("b", "b", vec![]),
        ];
        let graph = build_graph(&projects).unwrap();

        let path = find_path(&graph, "//a", "//b");
        assert!(path.is_none());
    }

    #[test]
    fn test_find_path_wrong_direction() {
        // A depends on B, but we search B -> A (wrong direction)
        let projects = vec![
            make_project("a", "a", vec![("b", "//b")]),
            make_project("b", "b", vec![]),
        ];
        let graph = build_graph(&projects).unwrap();

        let path = find_path(&graph, "//b", "//a");
        assert!(path.is_none());
    }

    #[test]
    fn test_find_path_to_self() {
        let projects = vec![make_project("a", "a", vec![])];
        let graph = build_graph(&projects).unwrap();

        let path = find_path(&graph, "//a", "//a");
        // A* should return the trivial path
        assert!(path.is_some());
        assert_eq!(path.unwrap(), vec!["//a"]);
    }

    #[test]
    fn test_find_path_nonexistent_project() {
        let projects = vec![make_project("a", "a", vec![])];
        let graph = build_graph(&projects).unwrap();

        assert!(find_path(&graph, "//a", "//nonexistent").is_none());
        assert!(find_path(&graph, "//nonexistent", "//a").is_none());
    }

    #[test]
    fn test_format_path() {
        let path = vec!["//a".to_string(), "//b".to_string(), "//c".to_string()];
        assert_eq!(format_path(&path), "//a -> //b -> //c");
    }

    #[test]
    fn test_format_path_single() {
        let path = vec!["//a".to_string()];
        assert_eq!(format_path(&path), "//a");
    }

    #[test]
    fn test_format_path_empty() {
        let path: Vec<String> = vec![];
        assert_eq!(format_path(&path), "");
    }

    #[test]
    fn test_find_shortest_path() {
        // A -> B -> D (path of 2)
        // A -> C -> D (path of 2)
        // Both are shortest paths, either is acceptable
        let projects = vec![
            make_project("a", "a", vec![("b", "//b"), ("c", "//c")]),
            make_project("b", "b", vec![("d", "//d")]),
            make_project("c", "c", vec![("d", "//d")]),
            make_project("d", "d", vec![]),
        ];
        let graph = build_graph(&projects).unwrap();

        let path = find_path(&graph, "//a", "//d");
        assert!(path.is_some());
        let path = path.unwrap();
        assert_eq!(path.len(), 3); // a -> (b or c) -> d
        assert_eq!(path[0], "//a");
        assert_eq!(path[2], "//d");
    }
}
