//! Target-level dependency graph construction and analysis
//!
//! Builds a petgraph DiGraph where nodes are targets (//project:target)
//! and edges represent dependencies between them based on Target.depends_on.

use std::collections::HashMap;

use petgraph::algo::astar;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::discovery::DiscoveredProject;

/// A node in the target dependency graph
#[derive(Debug, Clone)]
pub struct TargetNode {
    /// Full target address (//path/to/project:target)
    pub address: String,
    /// Project address (//path/to/project)
    pub project_address: String,
    /// Target name (test, build, deps, etc.)
    pub target_name: String,
    /// Command to execute
    pub command: String,
}

/// The dependency graph of targets
pub struct TargetGraph {
    /// The underlying directed graph
    pub graph: DiGraph<TargetNode, ()>,
    /// Map from target address to node index for fast lookups
    pub index_by_address: HashMap<String, NodeIndex>,
}

/// Build a target dependency graph from discovered projects
///
/// Creates nodes for each target in each project, and edges based on
/// the Target.depends_on field (which contains fully resolved addresses).
pub fn build_target_graph(projects: &[DiscoveredProject]) -> TargetGraph {
    let mut graph = DiGraph::new();
    let mut index_by_address = HashMap::new();

    // First pass: Add all targets as nodes
    for project in projects {
        let project_address = format!("//{}", project.relative_path.display());

        for (target_name, target) in &project.targets {
            let target_address = format!("{project_address}:{target_name}");

            let node = TargetNode {
                address: target_address.clone(),
                project_address: project_address.clone(),
                target_name: target_name.clone(),
                command: target.command.clone(),
            };

            let idx = graph.add_node(node);
            index_by_address.insert(target_address, idx);
        }
    }

    // Second pass: Add edges for dependencies
    for project in projects {
        let project_address = format!("//{}", project.relative_path.display());

        for (target_name, target) in &project.targets {
            let from_address = format!("{project_address}:{target_name}");

            if let Some(&from_idx) = index_by_address.get(&from_address) {
                for dep_address in &target.depends_on {
                    if let Some(&to_idx) = index_by_address.get(dep_address) {
                        // Add edge: dependent -> dependency
                        graph.add_edge(from_idx, to_idx, ());
                    }
                    // Missing dependencies are silently ignored (may be external)
                }
            }
        }
    }

    TargetGraph {
        graph,
        index_by_address,
    }
}

impl TargetGraph {
    /// Get a target node by address
    pub fn get(&self, address: &str) -> Option<&TargetNode> {
        self.index_by_address
            .get(address)
            .map(|idx| &self.graph[*idx])
    }

    /// Get all target nodes
    pub fn targets(&self) -> impl Iterator<Item = &TargetNode> {
        self.graph.node_weights()
    }

    /// Get direct dependencies of a target
    pub fn dependencies(&self, address: &str) -> Vec<&TargetNode> {
        if let Some(&idx) = self.index_by_address.get(address) {
            self.graph
                .neighbors(idx)
                .map(|dep_idx| &self.graph[dep_idx])
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get targets that depend ON this target (reverse dependencies)
    pub fn dependents(&self, address: &str) -> Vec<String> {
        use petgraph::Direction;

        if let Some(&idx) = self.index_by_address.get(address) {
            self.graph
                .neighbors_directed(idx, Direction::Incoming)
                .map(|dep_idx| self.graph[dep_idx].address.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Find the shortest dependency path between two targets
    ///
    /// Returns None if no path exists or if either target is not found.
    pub fn find_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let from_idx = self.index_by_address.get(from)?;
        let to_idx = self.index_by_address.get(to)?;

        let result = astar(&self.graph, *from_idx, |node| node == *to_idx, |_| 1, |_| 0);

        result.map(|(_, path)| {
            path.iter()
                .map(|idx| self.graph[*idx].address.clone())
                .collect()
        })
    }

    /// Check for cycles in the target graph using DFS
    pub fn find_cycle(&self) -> Option<CycleError> {
        use petgraph::visit::depth_first_search;
        use petgraph::visit::{Control, DfsEvent};

        let mut in_stack = vec![false; self.graph.node_count()];
        let mut path: Vec<NodeIndex> = Vec::new();

        let result = depth_first_search(&self.graph, self.graph.node_indices(), |event| {
            match event {
                DfsEvent::Discover(node, _) => {
                    in_stack[node.index()] = true;
                    path.push(node);
                }
                DfsEvent::TreeEdge(_, target) | DfsEvent::BackEdge(_, target) => {
                    if in_stack[target.index()] {
                        // Found a cycle - extract it
                        let cycle_start = path.iter().position(|&n| n == target).unwrap();
                        let cycle: Vec<String> = path[cycle_start..]
                            .iter()
                            .map(|&idx| self.graph[idx].address.clone())
                            .collect();
                        return Control::Break(cycle);
                    }
                }
                DfsEvent::Finish(node, _) => {
                    in_stack[node.index()] = false;
                    path.pop();
                }
                _ => {}
            }
            Control::Continue
        });

        match result {
            Control::Break(cycle) => Some(CycleError { cycle }),
            Control::Continue | Control::Prune => None,
        }
    }
}

/// Error indicating a dependency cycle was found
#[derive(Debug)]
pub struct CycleError {
    /// Target addresses forming the cycle
    pub cycle: Vec<String>,
}

impl std::fmt::Display for CycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dependency cycle detected: ")?;
        for (i, addr) in self.cycle.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{addr}")?;
        }
        write!(f, " -> {}", self.cycle[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{ProjectMetadata, Target};
    use std::path::PathBuf;

    fn make_project(
        relative_path: &str,
        targets: Vec<(&str, &str, Vec<&str>)>,
    ) -> DiscoveredProject {
        let project_address = format!("//{relative_path}");
        DiscoveredProject {
            root: PathBuf::from(format!("/workspace/{relative_path}")),
            config_path: PathBuf::from(format!("/workspace/{relative_path}/package.json")),
            metadata: ProjectMetadata {
                name: relative_path.split('/').next_back().unwrap().to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: vec![],
            targets: targets
                .into_iter()
                .map(|(name, cmd, deps)| {
                    (
                        name.to_string(),
                        Target {
                            command: cmd.to_string(),
                            depends_on: deps
                                .into_iter()
                                .map(|d| {
                                    // Handle //self: references for test convenience
                                    if d.starts_with("//self:") {
                                        format!("{}:{}", project_address, &d[7..])
                                    } else {
                                        d.to_string()
                                    }
                                })
                                .collect(),
                            capabilities: std::collections::HashSet::new(),
                            files_glob: None,
                        },
                    )
                })
                .collect(),
            plugin_name: "nodejs".to_string(),
            relative_path: PathBuf::from(relative_path),
        }
    }

    #[test]
    fn test_build_empty_graph() {
        let projects: Vec<DiscoveredProject> = vec![];
        let graph = build_target_graph(&projects);

        assert_eq!(graph.graph.node_count(), 0);
        assert_eq!(graph.graph.edge_count(), 0);
    }

    #[test]
    fn test_build_single_project_with_targets() {
        let projects = vec![make_project(
            "services/api",
            vec![
                ("deps", "npm install", vec![]),
                ("test", "npm test", vec!["//self:deps"]),
                ("build", "npm run build", vec!["//self:deps"]),
            ],
        )];

        let graph = build_target_graph(&projects);

        assert_eq!(graph.graph.node_count(), 3);
        assert_eq!(graph.graph.edge_count(), 2); // test->deps, build->deps

        // Check nodes exist
        assert!(graph.get("//services/api:deps").is_some());
        assert!(graph.get("//services/api:test").is_some());
        assert!(graph.get("//services/api:build").is_some());

        // Check dependencies
        let test_deps = graph.dependencies("//services/api:test");
        assert_eq!(test_deps.len(), 1);
        assert_eq!(test_deps[0].address, "//services/api:deps");
    }

    #[test]
    fn test_cross_project_dependencies() {
        let projects = vec![
            make_project(
                "libs/core",
                vec![
                    ("deps", "npm install", vec![]),
                    ("build", "npm run build", vec!["//self:deps"]),
                ],
            ),
            make_project(
                "services/api",
                vec![
                    ("deps", "npm install", vec![]),
                    (
                        "build",
                        "npm run build",
                        vec!["//self:deps", "//libs/core:build"],
                    ),
                    ("test", "npm test", vec!["//self:deps", "//libs/core:build"]),
                ],
            ),
        ];

        let graph = build_target_graph(&projects);

        assert_eq!(graph.graph.node_count(), 5);

        // api:build depends on core:build and api:deps
        let build_deps = graph.dependencies("//services/api:build");
        assert_eq!(build_deps.len(), 2);

        let dep_addrs: Vec<&str> = build_deps.iter().map(|d| d.address.as_str()).collect();
        assert!(dep_addrs.contains(&"//services/api:deps"));
        assert!(dep_addrs.contains(&"//libs/core:build"));
    }

    #[test]
    fn test_find_path_direct() {
        let projects = vec![make_project(
            "services/api",
            vec![
                ("deps", "npm install", vec![]),
                ("test", "npm test", vec!["//self:deps"]),
            ],
        )];

        let graph = build_target_graph(&projects);

        let path = graph.find_path("//services/api:test", "//services/api:deps");
        assert!(path.is_some());
        assert_eq!(
            path.unwrap(),
            vec!["//services/api:test", "//services/api:deps"]
        );
    }

    #[test]
    fn test_find_path_transitive() {
        let projects = vec![
            make_project(
                "libs/core",
                vec![
                    ("deps", "npm install", vec![]),
                    ("build", "npm run build", vec!["//self:deps"]),
                ],
            ),
            make_project(
                "services/api",
                vec![
                    ("deps", "npm install", vec![]),
                    ("test", "npm test", vec!["//self:deps", "//libs/core:build"]),
                ],
            ),
        ];

        let graph = build_target_graph(&projects);

        // api:test -> core:build -> core:deps
        let path = graph.find_path("//services/api:test", "//libs/core:deps");
        assert!(path.is_some());
        let path = path.unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], "//services/api:test");
        assert_eq!(path[1], "//libs/core:build");
        assert_eq!(path[2], "//libs/core:deps");
    }

    #[test]
    fn test_find_path_no_path() {
        let projects = vec![
            make_project("a", vec![("test", "test", vec![])]),
            make_project("b", vec![("test", "test", vec![])]),
        ];

        let graph = build_target_graph(&projects);

        let path = graph.find_path("//a:test", "//b:test");
        assert!(path.is_none());
    }

    #[test]
    fn test_dependents() {
        let projects = vec![make_project(
            "services/api",
            vec![
                ("deps", "npm install", vec![]),
                ("test", "npm test", vec!["//self:deps"]),
                ("build", "npm run build", vec!["//self:deps"]),
            ],
        )];

        let graph = build_target_graph(&projects);

        let deps_dependents = graph.dependents("//services/api:deps");
        assert_eq!(deps_dependents.len(), 2);
        assert!(deps_dependents.contains(&"//services/api:test".to_string()));
        assert!(deps_dependents.contains(&"//services/api:build".to_string()));
    }

    #[test]
    fn test_cycle_detection() {
        // Create a cycle: a:test -> b:test -> a:test
        let mut projects = vec![
            make_project("a", vec![("test", "test", vec!["//b:test"])]),
            make_project("b", vec![("test", "test", vec![])]),
        ];
        // Manually add the cycle
        projects[1].targets.get_mut("test").unwrap().depends_on = vec!["//a:test".to_string()];

        let graph = build_target_graph(&projects);

        let cycle = graph.find_cycle();
        assert!(cycle.is_some());
    }

    #[test]
    fn test_no_cycle() {
        let projects = vec![
            make_project(
                "libs/core",
                vec![
                    ("deps", "npm install", vec![]),
                    ("build", "npm run build", vec!["//self:deps"]),
                ],
            ),
            make_project(
                "services/api",
                vec![
                    ("deps", "npm install", vec![]),
                    ("test", "npm test", vec!["//self:deps", "//libs/core:build"]),
                ],
            ),
        ];

        let graph = build_target_graph(&projects);

        let cycle = graph.find_cycle();
        assert!(cycle.is_none());
    }
}
