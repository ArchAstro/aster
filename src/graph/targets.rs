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

        // Address order makes the reported rotation stable: the x/y fixture
        // always prints `//x:build -> //y:build -> //x:build`, independent of
        // HashMap insertion order. Acyclic graphs are unchanged.
        let mut starts: Vec<NodeIndex> = self.graph.node_indices().collect();
        starts.sort_by(|&left, &right| self.graph[left].address.cmp(&self.graph[right].address));
        let result = depth_first_search(&self.graph, starts, |event| {
            match event {
                DfsEvent::Discover(node, _) => {
                    in_stack[node.index()] = true;
                    path.push(node);
                }
                // A self-edge is a back edge to the node just discovered. The
                // path still holds only that node, so the cycle is `[addr]`
                // and Display prints `addr -> addr`.
                DfsEvent::BackEdge(source, target) if source == target => {
                    return Control::Break(vec![self.graph[source].address.clone()]);
                }
                DfsEvent::TreeEdge(_, target) | DfsEvent::BackEdge(_, target)
                    if in_stack[target.index()] =>
                {
                    // Found a cycle - extract it
                    let cycle_start = path.iter().position(|&n| n == target).unwrap();
                    let cycle: Vec<String> = path[cycle_start..]
                        .iter()
                        .map(|&idx| self.graph[idx].address.clone())
                        .collect();
                    return Control::Break(cycle);
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

/// The `selected_by` of each requested target that has one: the targets
/// whose runs it joins.
pub fn selected_by(
    projects: &[DiscoveredProject],
    addresses: &[String],
) -> HashMap<String, Vec<String>> {
    let mut found = HashMap::new();
    for addr in addresses {
        let Some((project_addr, name)) = addr.rsplit_once(':') else {
            continue;
        };
        let Some(target) = projects
            .iter()
            .find(|p| format!("//{}", p.relative_path.display()) == project_addr)
            .and_then(|p| p.targets.get(name))
        else {
            continue;
        };
        if !target.selected_by.is_empty() {
            found.insert(addr.clone(), target.selected_by.clone());
        }
    }
    found
}

/// The workspace-relative globs each of `addresses` reads outside its own
/// project: its `[targets.<name>.cache] workspace_include`, and any `include`
/// glob that climbs out of the project (`../../shared/*.json`).
///
/// These are file-level dependencies the target graph's edges cannot show.
/// Targets without any are omitted.
pub fn workspace_inputs(
    projects: &[DiscoveredProject],
    addresses: &[String],
) -> HashMap<String, Vec<String>> {
    let wanted: std::collections::HashSet<&str> = addresses.iter().map(String::as_str).collect();
    let mut inputs = HashMap::new();
    for project in projects {
        let project_address = format!("//{}", project.relative_path.display());
        for (target_name, target) in &project.targets {
            let address = format!("{project_address}:{target_name}");
            let Some(cache) = &target.cache else {
                continue;
            };
            if !wanted.contains(address.as_str()) {
                continue;
            }
            let mut globs = cache.workspace_include.clone();
            if let Some(scope) = &cache.cargo_workspace {
                globs.extend(scope.roots.iter().map(|member| {
                    member
                        .join(if scope.manifests_only {
                            "Cargo.toml"
                        } else {
                            "**"
                        })
                        .to_string_lossy()
                        .replace('\\', "/")
                }));
            }
            globs.extend(
                cache
                    .include
                    .iter()
                    .filter_map(|glob| escaping_glob(&project.relative_path, glob)),
            );
            if !globs.is_empty() {
                inputs.insert(address, globs);
            }
        }
    }
    inputs
}

/// The workspace-relative form of a project-relative `glob` that leaves the
/// project through a `..` anywhere in it (`../x`, `src/../../x`); `None` for
/// one that stays inside the project or climbs above the workspace root.
fn escaping_glob(project: &std::path::Path, glob: &str) -> Option<String> {
    if !glob.split('/').any(|part| part == "..") {
        return None;
    }
    let own: Vec<&str> = project
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let mut parts = own.clone();
    for part in glob.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    // `../<own dir>/x` climbs out and straight back in.
    (!parts.starts_with(&own)).then(|| parts.join("/"))
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
                                    if let Some(target) = d.strip_prefix("//self:") {
                                        format!("{project_address}:{target}")
                                    } else {
                                        d.to_string()
                                    }
                                })
                                .collect(),
                            capabilities: std::collections::HashSet::new(),
                            files_glob: None,
                            stream: false,
                            cache: None,
                            invalidates_cache: false,
                            working_dir: None,
                            exclusive_resources: vec![],
                            selected_by: vec![],
                            retry_setup: None,
                            report: None,
                        },
                    )
                })
                .collect(),
            plugin_name: "nodejs".to_string(),
            languages: vec!["nodejs".to_string()],
            build_system: None,
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
    fn self_dependency_is_a_cycle() {
        let projects = vec![make_project(
            "m",
            vec![("build", "echo m", vec!["//m:build"])],
        )];

        let cycle = build_target_graph(&projects)
            .find_cycle()
            .expect("a self-edge is a back edge to the current node");
        assert_eq!(cycle.cycle, vec!["//m:build".to_string()]);
        assert_eq!(
            cycle.to_string(),
            "Dependency cycle detected: //m:build -> //m:build"
        );
    }

    #[test]
    fn cycle_report_starts_at_the_lexicographically_first_address() {
        // Project order is the reverse of address order. The reported rotation
        // still matches the issue fixture.
        let projects = vec![
            make_project("y", vec![("build", "echo y", vec!["//x:build"])]),
            make_project("x", vec![("build", "echo x", vec!["//y:build"])]),
        ];

        let cycle = build_target_graph(&projects).find_cycle().unwrap();
        assert_eq!(
            cycle.cycle,
            vec!["//x:build".to_string(), "//y:build".to_string()]
        );
        assert_eq!(
            cycle.to_string(),
            "Dependency cycle detected: //x:build -> //y:build -> //x:build"
        );
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

    #[test]
    fn workspace_inputs_lists_cross_project_globs_of_requested_targets() {
        let mut app = make_project(
            "app",
            vec![("build", "make", vec![]), ("test", "make test", vec![])],
        );
        app.targets.get_mut("build").unwrap().cache = Some(crate::config::CacheConfig {
            workspace_include: vec!["libs/schema/*.json".to_string()],
            ..Default::default()
        });
        app.targets.get_mut("test").unwrap().cache = Some(crate::config::CacheConfig {
            include: vec!["fixtures/**".to_string()],
            ..Default::default()
        });
        let projects = vec![app];

        let all = workspace_inputs(
            &projects,
            &["//app:build".to_string(), "//app:test".to_string()],
        );
        assert_eq!(
            all.len(),
            1,
            "project-relative includes are not cross-project reads"
        );
        assert_eq!(all["//app:build"], vec!["libs/schema/*.json".to_string()]);

        let none = workspace_inputs(&projects, &["//app:test".to_string()]);
        assert!(none.is_empty());
    }

    #[test]
    fn workspace_inputs_resolves_include_globs_that_leave_the_project() {
        let mut lint = make_project("services/rust/cli", vec![("lint", "clippy", vec![])]);
        lint.targets.get_mut("lint").unwrap().cache = Some(crate::config::CacheConfig {
            include: vec![
                "crates/core/**".to_string(),
                "../../../src/ts/vectors.json".to_string(),
                "../cli/back-inside.json".to_string(),
                "src/../../shared/*.yaml".to_string(),
                "src/../tests/inside.rs".to_string(),
                "../../../../above-the-root.json".to_string(),
            ],
            ..Default::default()
        });

        let inputs = workspace_inputs(&[lint], &["//services/rust/cli:lint".to_string()]);
        assert_eq!(
            inputs["//services/rust/cli:lint"],
            vec![
                "src/ts/vectors.json".to_string(),
                "services/rust/shared/*.yaml".to_string()
            ]
        );
    }
}
