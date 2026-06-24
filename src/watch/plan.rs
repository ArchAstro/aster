//! Watch plan: resolves the set of watched targets `W = R ∪ deps(R)`,
//! builds per-target `TargetInputMatcher`s, and answers ownership queries
//! used by the event loop.

use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::cache::TargetInputMatcher;
use crate::discovery::DiscoveredProject;
use crate::graph::TargetGraph;
use crate::plugins::PluginRegistry;

/// A target inside the watch set with everything needed to decide ownership.
pub struct WatchTarget {
    pub address: String,
    pub project_address: String,
    pub project_root: PathBuf,
    pub target_name: String,
    pub matcher: TargetInputMatcher,
    /// True when the target declared no input globs and this matcher will
    /// always return false. The event loop falls back to whole-project-dir
    /// matching for these so users still get rebuilds.
    pub uses_fallback: bool,
    pub stream: bool,
}

pub struct WatchPlan {
    pub requested: HashSet<String>, // R
    pub watched: HashSet<String>,   // W = R ∪ deps(R)
    pub targets: Vec<WatchTarget>,
    pub watch_roots: Vec<PathBuf>,
    /// `(project_root, project_address)` sorted longest-first so a longest-prefix
    /// match finds the innermost owning project.
    pub project_by_root: Vec<(PathBuf, String)>,
}

impl WatchPlan {
    pub fn build(
        requested: &[String],
        projects: &[DiscoveredProject],
        target_graph: &TargetGraph,
        plugins: &PluginRegistry,
    ) -> Result<Self> {
        // Validate R exists in the graph.
        for addr in requested {
            if target_graph.get(addr).is_none() {
                return Err(anyhow!("target not found: {addr}"));
            }
        }

        let requested_set: HashSet<String> = requested.iter().cloned().collect();

        // W = R ∪ transitive deps.
        let mut watched = requested_set.clone();
        for addr in requested {
            collect_transitive_deps(addr, target_graph, &mut watched);
        }

        // project addr -> project
        let project_by_addr: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let mut targets = Vec::new();
        for addr in &watched {
            let node = target_graph
                .get(addr)
                .ok_or_else(|| anyhow!("target graph missing {addr}"))?;
            let project = project_by_addr
                .get(&node.project_address)
                .ok_or_else(|| anyhow!("project not found for {addr}"))?;
            let target_def = project
                .targets
                .get(&node.target_name)
                .ok_or_else(|| anyhow!("target def not found for {addr}"))?;

            let plugin = plugins.find_by_name(&project.plugin_name);
            let plugin_inputs = plugin
                .map(|p| p.cache_inputs(&node.target_name))
                .unwrap_or_default();

            let matcher =
                TargetInputMatcher::build(&project.root, &plugin_inputs, target_def.cache.as_ref())
                    .with_context(|| format!("failed to build input matcher for {addr}"))?;

            let uses_fallback = !matcher.has_patterns();

            targets.push(WatchTarget {
                address: addr.clone(),
                project_address: node.project_address.clone(),
                project_root: project.root.clone(),
                target_name: node.target_name.clone(),
                matcher,
                uses_fallback,
                stream: target_def.stream,
            });
        }

        // Distinct project roots for targets in W — these are our fs-watch roots.
        let mut watch_roots: Vec<PathBuf> =
            targets.iter().map(|t| t.project_root.clone()).collect();
        watch_roots.sort();
        watch_roots.dedup();

        // Sorted longest-first for ownership lookup.
        let mut project_by_root: Vec<(PathBuf, String)> = targets
            .iter()
            .map(|t| (t.project_root.clone(), t.project_address.clone()))
            .collect();
        project_by_root.sort_by_key(|p| std::cmp::Reverse(p.0.as_os_str().len()));
        project_by_root.dedup_by(|a, b| a.0 == b.0);

        Ok(Self {
            requested: requested_set,
            watched,
            targets,
            watch_roots,
            project_by_root,
        })
    }

    /// Find the innermost project owning the absolute path.
    pub fn owning_project(&self, abs_path: &Path) -> Option<&str> {
        self.project_by_root
            .iter()
            .find(|(root, _)| abs_path.starts_with(root))
            .map(|(_, addr)| addr.as_str())
    }

    /// Every target in W whose `cache_inputs` own the given absolute path.
    /// Falls back to "any non-suppressed change in project dir" for targets
    /// that declared no inputs.
    pub fn owners_of(&self, abs_path: &Path) -> Vec<&WatchTarget> {
        let Some(proj_addr) = self.owning_project(abs_path) else {
            return Vec::new();
        };
        self.targets
            .iter()
            .filter(|t| t.project_address == proj_addr)
            .filter(|t| {
                if t.uses_fallback {
                    // Fallback: any change inside the project dir counts.
                    true
                } else {
                    t.matcher.owns(abs_path)
                }
            })
            .collect()
    }

    /// Expand owners into the primary set: each owner plus every target in W
    /// that transitively depends on it.
    pub fn primary_set(&self, owners: &[&WatchTarget], graph: &TargetGraph) -> HashSet<String> {
        let mut primary: HashSet<String> = HashSet::new();
        for owner in owners {
            if self.watched.contains(&owner.address) {
                primary.insert(owner.address.clone());
            }
            collect_transitive_dependents(&owner.address, graph, &self.watched, &mut primary);
        }
        primary
    }

    pub fn has_stream_targets(&self) -> bool {
        self.targets
            .iter()
            .any(|t| t.stream && self.requested.contains(&t.address))
    }
}

fn collect_transitive_deps(addr: &str, graph: &TargetGraph, out: &mut HashSet<String>) {
    for dep in graph.dependencies(addr) {
        if out.insert(dep.address.clone()) {
            collect_transitive_deps(&dep.address, graph, out);
        }
    }
}

fn collect_transitive_dependents(
    addr: &str,
    graph: &TargetGraph,
    bounded_by: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    for dep_addr in graph.dependents(addr) {
        if bounded_by.contains(&dep_addr) && out.insert(dep_addr.clone()) {
            collect_transitive_dependents(&dep_addr, graph, bounded_by, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_target_graph;
    use crate::plugins::{
        CacheInputs, LanguagePlugin, LocalDependency, NodeJsPlugin, ProjectMetadata, Target,
        TargetContext,
    };
    use anyhow::Result as AnyResult;
    use std::collections::HashMap as StdHashMap;

    fn mk_project(rel: &str, abs_root: &str, targets: &[(&str, Vec<&str>)]) -> DiscoveredProject {
        let mut target_map = StdHashMap::new();
        for (name, deps) in targets {
            target_map.insert(
                name.to_string(),
                Target {
                    command: format!("echo {name}"),
                    depends_on: deps.iter().map(|d| d.to_string()).collect(),
                    ..Default::default()
                },
            );
        }
        DiscoveredProject {
            root: PathBuf::from(abs_root),
            config_path: PathBuf::from(format!("{abs_root}/package.json")),
            metadata: ProjectMetadata {
                name: rel.split('/').next_back().unwrap().to_string(),
                version: None,
            },
            dependencies: vec![],
            targets: target_map,
            plugin_name: "nodejs".to_string(),
            relative_path: PathBuf::from(rel),
        }
    }

    fn registry() -> PluginRegistry {
        let mut r = PluginRegistry::new();
        r.register(Box::new(NodeJsPlugin));
        r
    }

    #[test]
    fn watched_expands_to_transitive_deps() {
        let projects = vec![
            mk_project(
                "services/api",
                "/repo/services/api",
                &[("build", vec!["//libs/core:build"])],
            ),
            mk_project(
                "libs/core",
                "/repo/libs/core",
                &[("build", vec!["//libs/util:build"])],
            ),
            mk_project("libs/util", "/repo/libs/util", &[("build", vec![])]),
        ];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &["//services/api:build".to_string()],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();

        assert_eq!(plan.requested.len(), 1);
        assert!(plan.watched.contains("//services/api:build"));
        assert!(plan.watched.contains("//libs/core:build"));
        assert!(plan.watched.contains("//libs/util:build"));
        assert_eq!(plan.watch_roots.len(), 3);
    }

    #[test]
    fn owning_project_picks_innermost() {
        let projects = vec![
            mk_project("libs", "/repo/libs", &[("build", vec![])]),
            mk_project("libs/core", "/repo/libs/core", &[("build", vec![])]),
        ];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &["//libs:build".to_string(), "//libs/core:build".to_string()],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();

        assert_eq!(
            plan.owning_project(Path::new("/repo/libs/core/src/foo.ts")),
            Some("//libs/core"),
        );
        assert_eq!(
            plan.owning_project(Path::new("/repo/libs/helpers.ts")),
            Some("//libs"),
        );
    }

    #[test]
    fn owners_matches_against_cache_inputs() {
        let projects = vec![mk_project(
            "libs/core",
            "/repo/libs/core",
            &[("build", vec![])],
        )];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &["//libs/core:build".to_string()],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();

        // Node plugin's build inputs include src/**/*.ts; readme shouldn't match.
        let ts_owners = plan.owners_of(Path::new("/repo/libs/core/src/foo.ts"));
        assert_eq!(ts_owners.len(), 1);
        assert_eq!(ts_owners[0].address, "//libs/core:build");

        let md_owners = plan.owners_of(Path::new("/repo/libs/core/README.md"));
        assert!(md_owners.is_empty());
    }

    #[test]
    fn primary_set_includes_transitive_dependents_in_w() {
        let projects = vec![
            mk_project(
                "services/api",
                "/repo/services/api",
                &[("build", vec!["//libs/core:build"])],
            ),
            mk_project("libs/core", "/repo/libs/core", &[("build", vec![])]),
        ];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &["//services/api:build".to_string()],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();

        // Pretend libs/core:build is the owner.
        let core = plan
            .targets
            .iter()
            .find(|t| t.address == "//libs/core:build")
            .unwrap();
        let primary = plan.primary_set(&[core], &graph);

        assert!(primary.contains("//libs/core:build"));
        assert!(primary.contains("//services/api:build"));
        assert_eq!(primary.len(), 2);
    }

    /// Plugin whose cache_inputs is always empty — simulates a custom plugin
    /// or a target whose language declared nothing.
    struct EmptyInputsPlugin;

    impl LanguagePlugin for EmptyInputsPlugin {
        fn name(&self) -> &str {
            "empty"
        }
        fn marker_files(&self) -> &[&str] {
            &[]
        }
        fn parse_project(&self, _: &Path, _: &Path) -> AnyResult<ProjectMetadata> {
            Ok(ProjectMetadata {
                name: "p".into(),
                version: None,
            })
        }
        fn parse_dependencies(&self, _: &Path) -> AnyResult<Vec<LocalDependency>> {
            Ok(vec![])
        }
        fn detect_targets(&self, _: &TargetContext) -> AnyResult<StdHashMap<String, Target>> {
            Ok(StdHashMap::new())
        }
        fn cache_inputs(&self, _: &str) -> CacheInputs {
            CacheInputs::default()
        }
    }

    #[test]
    fn target_with_empty_inputs_uses_fallback() {
        let mut projects = vec![mk_project(
            "custom/proj",
            "/repo/custom/proj",
            &[("build", vec![])],
        )];
        // Override plugin name so the EmptyInputsPlugin is selected.
        projects[0].plugin_name = "empty".to_string();

        let graph = build_target_graph(&projects);
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(EmptyInputsPlugin));

        let plan = WatchPlan::build(
            &["//custom/proj:build".to_string()],
            &projects,
            &graph,
            &registry,
        )
        .unwrap();

        let target = plan
            .targets
            .iter()
            .find(|t| t.address == "//custom/proj:build")
            .unwrap();
        assert!(
            target.uses_fallback,
            "target with empty cache_inputs must use fallback"
        );

        // Fallback means *any* file inside the project dir counts as an owner.
        let owners = plan.owners_of(Path::new("/repo/custom/proj/anything.xyz"));
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].address, "//custom/proj:build");

        // Even totally unrelated extensions fire, which is the correct
        // degraded behavior — users must add [targets.X.cache] to refine.
        let owners2 = plan.owners_of(Path::new("/repo/custom/proj/README.md"));
        assert_eq!(owners2.len(), 1);

        // But paths outside the project still don't match.
        let owners3 = plan.owners_of(Path::new("/repo/other/foo.ts"));
        assert!(owners3.is_empty());
    }

    #[test]
    fn fallback_and_real_matcher_coexist() {
        // One project with a real-inputs plugin, another with empty inputs.
        // Make sure the fallback path is scoped per-target, not globally.
        let mut projects = vec![
            mk_project("a", "/repo/a", &[("build", vec![])]),
            mk_project("b", "/repo/b", &[("build", vec![])]),
        ];
        projects[1].plugin_name = "empty".to_string();

        let graph = build_target_graph(&projects);
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(NodeJsPlugin));
        registry.register(Box::new(EmptyInputsPlugin));

        let plan = WatchPlan::build(
            &["//a:build".to_string(), "//b:build".to_string()],
            &projects,
            &graph,
            &registry,
        )
        .unwrap();

        let a = plan
            .targets
            .iter()
            .find(|t| t.address == "//a:build")
            .unwrap();
        let b = plan
            .targets
            .iter()
            .find(|t| t.address == "//b:build")
            .unwrap();
        assert!(!a.uses_fallback, "nodejs plugin provides inputs");
        assert!(b.uses_fallback);

        // Non-input file in A: no match.
        assert!(plan.owners_of(Path::new("/repo/a/README.md")).is_empty());
        // Same filename in B: matches because B is in fallback mode.
        assert_eq!(plan.owners_of(Path::new("/repo/b/README.md")).len(), 1);
    }

    #[test]
    fn primary_set_bounded_by_w() {
        let projects = vec![
            mk_project(
                "services/api",
                "/repo/services/api",
                &[("build", vec!["//libs/core:build"])],
            ),
            mk_project("libs/core", "/repo/libs/core", &[("build", vec![])]),
            mk_project(
                "services/worker",
                "/repo/services/worker",
                &[("build", vec!["//libs/core:build"])],
            ),
        ];
        let graph = build_target_graph(&projects);

        // W only covers api's closure — worker depends on core too but should not be rebuilt.
        let plan = WatchPlan::build(
            &["//services/api:build".to_string()],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();

        let core = plan
            .targets
            .iter()
            .find(|t| t.address == "//libs/core:build")
            .unwrap();
        let primary = plan.primary_set(&[core], &graph);

        assert!(primary.contains("//libs/core:build"));
        assert!(primary.contains("//services/api:build"));
        assert!(!primary.contains("//services/worker:build"));
    }
}
