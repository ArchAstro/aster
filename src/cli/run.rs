//! Run command argument parsing and project selection
//!
//! Handles parsing external subcommand args for target execution
//! and selecting projects based on flags (--all, --no-deps, --dependents).

use std::collections::HashSet;
use std::path::Path;

use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;

/// Reserved command names that cannot be used as targets
pub const RESERVED_COMMANDS: &[&str] =
    &["list", "graph", "why", "init", "affected", "logs", "help"];

/// Parsed arguments for the run command
#[derive(Debug, Clone, Default)]
pub struct RunArgs {
    /// Target to run (e.g., "test", "build", "lint")
    pub target: String,
    /// Explicit projects to run on (//path/to/project, //prefix/..., ./...)
    pub projects: Vec<String>,
    /// Projects to exclude (-//path/to/project, -//prefix/...)
    pub exclusions: Vec<String>,
    /// Skip dependencies (run only specified projects)
    pub no_deps: bool,
    /// Include projects that depend on selected projects
    pub dependents: bool,
    /// Run on all projects
    pub all: bool,
    /// Use current directory to find project (triggered by "." argument)
    pub use_cwd: bool,
    /// Treat warnings as errors for targets that support it
    pub warnings_as_errors: bool,
    /// Stream override: Some(true) = force stream, Some(false) = force no-stream, None = use target config
    pub stream_override: Option<bool>,
    // Global flags that clap can't parse from external subcommands
    /// Disable caching
    pub no_cache: bool,
    /// Enable verbose output
    pub verbose: bool,
    /// Suppress per-project output
    pub quiet: bool,
    /// Output in JSON format
    pub json: bool,
    /// Show full output for failed targets
    pub full_logs: bool,
}

/// Check if a target name conflicts with a reserved command
///
/// Returns an error message if the target is reserved, None otherwise.
pub fn check_reserved_target(target: &str) -> Option<String> {
    if RESERVED_COMMANDS.contains(&target) {
        Some(format!(
            "'{target}' is a reserved command. If you have a target named '{target}', \
             rename it to avoid conflicts (e.g., 'run-{target}' or '{target}-all').\n\n\
             Reserved commands: {}",
            RESERVED_COMMANDS.join(", ")
        ))
    } else {
        None
    }
}

/// Parse external subcommand args into RunArgs
///
/// Args format: [target] [projects...] [--no-deps] [--dependents] [--all]
///
/// Project patterns:
///   //path/to/project  - exact project match
///   //path/prefix/...  - all projects under prefix
///   //...              - all projects (same as --all)
///   ./...              - all projects under current directory
///   -//path/...        - exclude from selection
///
/// Examples:
///   ["test"]                     -> target=test, all projects
///   ["test", "//a", "//b"]       -> target=test, projects a and b (+ deps)
///   ["test", "--all"]            -> target=test, all projects
///   ["test", "//a", "--no-deps"] -> target=test, only project a
///   ["test", "//a", "--dependents"] -> target=test, a + its dependents
///   ["test", "//...", "-//vendor/..."] -> all projects except under vendor/
pub fn parse_run_args(args: Vec<String>) -> RunArgs {
    let mut target = String::new();
    let mut projects = Vec::new();
    let mut exclusions = Vec::new();
    let mut no_deps = false;
    let mut dependents = false;
    let mut all = false;
    let mut use_cwd = false;
    let mut warnings_as_errors = false;
    let mut stream_override = None;
    let mut no_cache = false;
    let mut verbose = false;
    let mut quiet = false;
    let mut json = false;
    let mut full_logs = false;

    for arg in args {
        match arg.as_str() {
            "--no-deps" => no_deps = true,
            "--dependents" => dependents = true,
            "--all" => all = true,
            "--warnings-as-errors" => warnings_as_errors = true,
            "--stream" => stream_override = Some(true),
            "--no-stream" => stream_override = Some(false),
            // Global flags that clap can't parse from external subcommands
            "--no-cache" => no_cache = true,
            "--verbose" | "-v" => verbose = true,
            "--quiet" | "-q" => quiet = true,
            "--json" => json = true,
            "--full-logs" => full_logs = true,
            "." => {
                // Current directory - use cwd detection
                use_cwd = true;
            }
            _ if arg.starts_with("--") => {
                // Unknown flag - ignore for now
            }
            _ if arg.starts_with("-//") => {
                // Exclusion pattern (e.g., -//vendor/...)
                exclusions.push(arg[1..].to_string()); // Strip leading "-"
            }
            _ if arg.starts_with("//") => {
                // Project address or glob
                projects.push(arg);
            }
            _ if arg.starts_with("./") => {
                // Relative path pattern (e.g., ./...)
                projects.push(arg);
            }
            _ if target.is_empty() => {
                // First positional arg is target
                target = arg;
            }
            _ => {
                // Additional positional args could be projects without //
                // For now, require // prefix
            }
        }
    }

    RunArgs {
        target,
        projects,
        exclusions,
        no_deps,
        dependents,
        all,
        use_cwd,
        warnings_as_errors,
        stream_override,
        no_cache,
        verbose,
        quiet,
        json,
        full_logs,
    }
}

/// Select initial projects based on args
///
/// Priority:
/// 1. If --all or //...: return all projects (minus exclusions)
/// 2. If explicit projects given: return those (supports glob syntax)
/// 3. Otherwise: try to find project in cwd
///
/// Glob syntax:
/// - `//path/to/project` - exact match
/// - `//path/prefix/...` - all projects under that path prefix
/// - `//...` - all projects
/// - `./...` - all projects under current directory
/// - `-//path/...` - exclude from selection (in args.exclusions)
pub fn select_projects<'a>(
    args: &RunArgs,
    graph: &'a ProjectGraph,
    discovered: &'a [DiscoveredProject],
    cwd: &Path,
    workspace_root: &Path,
) -> Result<Vec<&'a DiscoveredProject>, String> {
    // Map addresses to discovered projects
    let project_by_addr: std::collections::HashMap<String, &DiscoveredProject> = discovered
        .iter()
        .map(|p| (format!("//{}", p.relative_path.display()), p))
        .collect();

    // Get cwd relative to workspace for ./... pattern
    let relative_cwd = cwd.strip_prefix(workspace_root).ok();

    // Helper to check if a pattern matches a project address
    let matches_pattern = |pattern: &str, project_addr: &str| -> bool {
        if pattern == "//..." {
            // Match all projects
            true
        } else if let Some(prefix) = pattern.strip_suffix("/...") {
            // Glob pattern: //prefix/... or ./...
            let abs_prefix = if prefix == "." {
                // ./... - use cwd
                relative_cwd
                    .map(|p| format!("//{}", p.display()))
                    .unwrap_or_default()
            } else if prefix.starts_with("./") {
                // ./path/... - relative to cwd
                relative_cwd
                    .map(|p| format!("//{}/{}", p.display(), &prefix[2..]))
                    .unwrap_or_default()
            } else {
                prefix.to_string()
            };

            if abs_prefix.is_empty() {
                return false;
            }

            let prefix_with_slash = format!("{abs_prefix}/");
            project_addr == abs_prefix || project_addr.starts_with(&prefix_with_slash)
        } else {
            // Exact match
            project_addr == pattern
        }
    };

    // Check for //... in projects (means all)
    let select_all = args.all || args.projects.iter().any(|p| p == "//...");

    let mut result: Vec<&DiscoveredProject>;
    let mut seen = HashSet::new();

    if select_all {
        // Start with all projects
        result = discovered.iter().collect();
        for p in &result {
            seen.insert(format!("//{}", p.relative_path.display()));
        }
    } else if !args.projects.is_empty() {
        // Return explicitly specified projects
        result = Vec::new();

        for pattern in &args.projects {
            if pattern == "//..." {
                continue; // Already handled above
            }

            let mut found_any = false;

            for p in discovered {
                let project_addr = format!("//{}", p.relative_path.display());

                if matches_pattern(pattern, &project_addr) && seen.insert(project_addr) {
                    result.push(p);
                    found_any = true;
                }
            }

            // For exact matches (non-glob), verify the project exists
            if !found_any && !pattern.ends_with("/...") {
                // Check if it's in the graph but just not matched
                if graph.get(pattern).is_none() && !project_by_addr.contains_key(pattern) {
                    return Err(format!("Project not found: {pattern}"));
                }
            }

            if !found_any && pattern.ends_with("/...") {
                return Err(format!("No projects found matching: {pattern}"));
            }
        }
    } else {
        // Try to detect project from cwd
        if let Some(rel_cwd) = relative_cwd {
            let mut best_match: Option<&DiscoveredProject> = None;
            let mut best_match_len = 0;

            for project in discovered {
                let proj_path = &project.relative_path;
                // Handle root project (empty path) specially - only match if we're at root
                let is_match = if proj_path.as_os_str().is_empty() {
                    rel_cwd.as_os_str().is_empty()
                } else {
                    rel_cwd.starts_with(proj_path)
                };
                if is_match {
                    let path_len = proj_path.as_os_str().len();
                    if path_len > best_match_len || best_match.is_none() {
                        best_match = Some(project);
                        best_match_len = path_len;
                    }
                }
            }

            if let Some(project) = best_match {
                return Ok(vec![project]);
            }
        }

        return Err(
            "No projects specified. Use --all to run on all projects, or specify project addresses."
                .to_string(),
        );
    }

    // Apply exclusions
    if !args.exclusions.is_empty() {
        result.retain(|p| {
            let project_addr = format!("//{}", p.relative_path.display());
            !args
                .exclusions
                .iter()
                .any(|excl| matches_pattern(excl, &project_addr))
        });
    }

    Ok(result)
}

/// Expand project selection based on flags
///
/// - Default: include dependencies (build deps first)
/// - --no-deps: don't include dependencies
/// - --dependents: include projects that depend on selected projects
pub fn expand_selection<'a>(
    args: &RunArgs,
    initial: &[&'a DiscoveredProject],
    graph: &ProjectGraph,
    discovered: &'a [DiscoveredProject],
) -> Vec<&'a DiscoveredProject> {
    let mut selected: HashSet<String> = initial
        .iter()
        .map(|p| format!("//{}", p.relative_path.display()))
        .collect();

    // If --dependents, add reverse dependencies
    if args.dependents {
        let initial_addrs: Vec<String> = initial
            .iter()
            .map(|p| format!("//{}", p.relative_path.display()))
            .collect();

        for addr in &initial_addrs {
            for dep_addr in graph.dependents(addr) {
                selected.insert(dep_addr);
            }
        }
    }

    // If NOT --no-deps, add dependencies
    if !args.no_deps {
        // Collect all transitive dependencies
        let initial_addrs: Vec<String> = selected.iter().cloned().collect();
        for addr in &initial_addrs {
            collect_deps_recursive(addr, graph, &mut selected);
        }
    }

    // Convert back to project references
    discovered
        .iter()
        .filter(|p| selected.contains(&format!("//{}", p.relative_path.display())))
        .collect()
}

/// Recursively collect dependencies
fn collect_deps_recursive(addr: &str, graph: &ProjectGraph, collected: &mut HashSet<String>) {
    for dep in graph.dependencies(addr) {
        if collected.insert(dep.address.clone()) {
            collect_deps_recursive(&dep.address, graph, collected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_run_args_target_only() {
        let args = parse_run_args(vec!["test".to_string()]);

        assert_eq!(args.target, "test");
        assert!(args.projects.is_empty());
        assert!(!args.no_deps);
        assert!(!args.dependents);
        assert!(!args.all);
        assert!(!args.use_cwd);
        assert!(!args.warnings_as_errors);
    }

    #[test]
    fn test_parse_run_args_with_projects() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "//services/api".to_string(),
            "//libs/core".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert_eq!(args.projects, vec!["//services/api", "//libs/core"]);
        assert!(!args.no_deps);
        assert!(!args.all);
    }

    #[test]
    fn test_parse_run_args_with_all_flag() {
        let args = parse_run_args(vec!["build".to_string(), "--all".to_string()]);

        assert_eq!(args.target, "build");
        assert!(args.projects.is_empty());
        assert!(args.all);
    }

    #[test]
    fn test_parse_run_args_with_no_deps() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "//a".to_string(),
            "--no-deps".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert_eq!(args.projects, vec!["//a"]);
        assert!(args.no_deps);
        assert!(!args.dependents);
    }

    #[test]
    fn test_parse_run_args_with_dependents() {
        let args = parse_run_args(vec![
            "lint".to_string(),
            "//libs/core".to_string(),
            "--dependents".to_string(),
        ]);

        assert_eq!(args.target, "lint");
        assert_eq!(args.projects, vec!["//libs/core"]);
        assert!(args.dependents);
    }

    #[test]
    fn test_parse_run_args_combined_flags() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "//a".to_string(),
            "--no-deps".to_string(),
            "--dependents".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert!(args.no_deps);
        assert!(args.dependents);
    }

    #[test]
    fn test_parse_run_args_empty() {
        let args = parse_run_args(vec![]);

        assert_eq!(args.target, "");
        assert!(args.projects.is_empty());
    }

    #[test]
    fn test_parse_run_args_with_dot() {
        let args = parse_run_args(vec!["test".to_string(), ".".to_string()]);

        assert_eq!(args.target, "test");
        assert!(args.projects.is_empty());
        assert!(args.use_cwd);
    }

    #[test]
    fn test_check_reserved_target_allows_normal_targets() {
        assert!(check_reserved_target("test").is_none());
        assert!(check_reserved_target("build").is_none());
        assert!(check_reserved_target("lint").is_none());
        assert!(check_reserved_target("custom-target").is_none());
    }

    #[test]
    fn test_check_reserved_target_blocks_reserved_commands() {
        assert!(check_reserved_target("list").is_some());
        assert!(check_reserved_target("graph").is_some());
        assert!(check_reserved_target("why").is_some());
        assert!(check_reserved_target("init").is_some());
        assert!(check_reserved_target("affected").is_some());
        assert!(check_reserved_target("logs").is_some());
        assert!(check_reserved_target("help").is_some());
    }

    #[test]
    fn test_parse_run_args_with_warnings_as_errors() {
        let args = parse_run_args(vec![
            "build".to_string(),
            "--all".to_string(),
            "--warnings-as-errors".to_string(),
        ]);

        assert_eq!(args.target, "build");
        assert!(args.all);
        assert!(args.warnings_as_errors);
    }

    #[test]
    fn test_parse_run_args_with_glob_pattern() {
        let args = parse_run_args(vec!["test".to_string(), "//src/ts/...".to_string()]);

        assert_eq!(args.target, "test");
        assert_eq!(args.projects, vec!["//src/ts/..."]);
    }

    #[test]
    fn test_parse_run_args_with_multiple_glob_patterns() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "//src/ts/...".to_string(),
            "//libs/...".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert_eq!(args.projects, vec!["//src/ts/...", "//libs/..."]);
    }

    #[test]
    fn test_parse_run_args_with_exclusions() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "//...".to_string(),
            "-//vendor/...".to_string(),
            "-//generated".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert_eq!(args.projects, vec!["//..."]);
        assert_eq!(args.exclusions, vec!["//vendor/...", "//generated"]);
    }

    #[test]
    fn test_parse_run_args_with_relative_glob() {
        let args = parse_run_args(vec!["test".to_string(), "./...".to_string()]);

        assert_eq!(args.target, "test");
        assert_eq!(args.projects, vec!["./..."]);
    }

    #[test]
    fn test_parse_run_args_with_stream_flag() {
        let args = parse_run_args(vec![
            "run-dev".to_string(),
            ".".to_string(),
            "--stream".to_string(),
        ]);

        assert_eq!(args.target, "run-dev");
        assert!(args.use_cwd);
        assert_eq!(args.stream_override, Some(true));
    }

    #[test]
    fn test_parse_run_args_with_no_stream_flag() {
        let args = parse_run_args(vec!["run-dev".to_string(), "--no-stream".to_string()]);

        assert_eq!(args.target, "run-dev");
        assert_eq!(args.stream_override, Some(false));
    }

    #[test]
    fn test_parse_run_args_default_stream_override_is_none() {
        let args = parse_run_args(vec!["test".to_string()]);

        assert_eq!(args.stream_override, None);
    }

    #[test]
    fn test_parse_run_args_global_flags() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "--all".to_string(),
            "--no-cache".to_string(),
            "--verbose".to_string(),
            "--full-logs".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert!(args.all);
        assert!(args.no_cache);
        assert!(args.verbose);
        assert!(args.full_logs);
        assert!(!args.quiet);
        assert!(!args.json);
    }

    #[test]
    fn test_parse_run_args_global_flags_short() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "-v".to_string(),
            "-q".to_string(),
            "--json".to_string(),
        ]);

        assert!(args.verbose);
        assert!(args.quiet);
        assert!(args.json);
    }

    mod select_projects_tests {
        use super::*;
        use crate::discovery::DiscoveredProject;
        use crate::graph::build_graph;
        use crate::plugins::ProjectMetadata;
        use std::collections::HashMap;
        use std::path::PathBuf;

        fn make_project(name: &str, relative_path: &str) -> DiscoveredProject {
            DiscoveredProject {
                root: PathBuf::from("/workspace").join(relative_path),
                config_path: PathBuf::from("/workspace")
                    .join(relative_path)
                    .join("package.json"),
                metadata: ProjectMetadata {
                    name: name.to_string(),
                    version: None,
                },
                dependencies: vec![],
                targets: HashMap::new(),
                plugin_name: "nodejs".to_string(),
                relative_path: PathBuf::from(relative_path),
            }
        }

        #[test]
        fn test_select_projects_glob_matches_prefix() {
            let projects = vec![
                make_project("ts-app", "src/ts/app"),
                make_project("ts-lib", "src/ts/lib"),
                make_project("go-service", "src/go/service"),
                make_project("shared", "libs/shared"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//src/ts/...".to_string()],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"ts-app"));
            assert!(names.contains(&"ts-lib"));
        }

        #[test]
        fn test_select_projects_glob_no_matches_returns_error() {
            let projects = vec![
                make_project("app", "src/app"),
                make_project("lib", "src/lib"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//nonexistent/...".to_string()],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let result = select_projects(&args, &graph, &projects, cwd, workspace_root);

            assert!(result.is_err());
            assert!(result.unwrap_err().contains("No projects found matching"));
        }

        #[test]
        fn test_select_projects_glob_matches_exact_prefix() {
            // Test that //libs/... also matches //libs (exact match at prefix level)
            let projects = vec![
                make_project("libs-root", "libs"),
                make_project("libs-sub", "libs/shared"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//libs/...".to_string()],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"libs-root"));
            assert!(names.contains(&"libs-sub"));
        }

        #[test]
        fn test_select_projects_mixed_glob_and_exact() {
            let projects = vec![
                make_project("ts-app", "src/ts/app"),
                make_project("ts-lib", "src/ts/lib"),
                make_project("specific", "other/specific"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//src/ts/...".to_string(), "//other/specific".to_string()],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 3);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"ts-app"));
            assert!(names.contains(&"ts-lib"));
            assert!(names.contains(&"specific"));
        }

        #[test]
        fn test_select_projects_glob_deduplicates() {
            let projects = vec![
                make_project("app", "src/app"),
                make_project("lib", "src/lib"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            // Use overlapping patterns that would match the same projects
            let args = RunArgs {
                target: "test".to_string(),
                projects: vec![
                    "//src/...".to_string(),
                    "//src/app".to_string(), // Already included in //src/...
                ],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            // Should only have 2 projects, not 3
            assert_eq!(selected.len(), 2);
        }

        #[test]
        fn test_select_projects_all_shorthand() {
            // //... should select all projects
            let projects = vec![
                make_project("app", "src/app"),
                make_project("lib", "libs/lib"),
                make_project("other", "other"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//...".to_string()],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 3);
        }

        #[test]
        fn test_select_projects_exclusion() {
            // //... with exclusion -//libs/...
            let projects = vec![
                make_project("app", "src/app"),
                make_project("lib", "libs/lib"),
                make_project("util", "libs/util"),
                make_project("other", "other"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//...".to_string()],
                exclusions: vec!["//libs/...".to_string()],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"app"));
            assert!(names.contains(&"other"));
            assert!(!names.contains(&"lib"));
            assert!(!names.contains(&"util"));
        }

        #[test]
        fn test_select_projects_exclusion_exact() {
            // Exclude a specific project
            let projects = vec![
                make_project("app", "src/app"),
                make_project("lib", "src/lib"),
                make_project("slow", "src/slow"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["//src/...".to_string()],
                exclusions: vec!["//src/slow".to_string()],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"app"));
            assert!(names.contains(&"lib"));
            assert!(!names.contains(&"slow"));
        }

        #[test]
        fn test_select_projects_relative_glob() {
            // ./... from within src/ts should select src/ts projects
            let projects = vec![
                make_project("ts-app", "src/ts/app"),
                make_project("ts-lib", "src/ts/lib"),
                make_project("go-svc", "src/go/svc"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace/src/ts");

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec!["./...".to_string()],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: false,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"ts-app"));
            assert!(names.contains(&"ts-lib"));
        }

        #[test]
        fn test_select_projects_cwd_detection_prefers_nested_over_root() {
            // When in a subdirectory, should NOT match root project with empty path
            let projects = vec![
                make_project("root", ""), // Root project with empty path
                make_project("nested", "services/api"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace/services/api");

            // No explicit projects - rely on cwd detection
            let args = RunArgs {
                target: "test".to_string(),
                projects: vec![],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: true,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            // Should select nested project, NOT root
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].metadata.name, "nested");
        }

        #[test]
        fn test_select_projects_cwd_detection_no_match_returns_error() {
            // When in a subdirectory with no matching project, should error not match root
            let projects = vec![
                make_project("root", ""), // Root project with empty path
                make_project("other", "other/project"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace/services/api"); // No project here

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec![],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: true,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let result = select_projects(&args, &graph, &projects, cwd, workspace_root);

            // Should error, not match root
            assert!(result.is_err());
        }

        #[test]
        fn test_select_projects_cwd_detection_at_root_matches_root() {
            // When at workspace root, should match root project
            let projects = vec![
                make_project("root", ""),
                make_project("nested", "services/api"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace"); // At root

            let args = RunArgs {
                target: "test".to_string(),
                projects: vec![],
                exclusions: vec![],
                no_deps: true,
                dependents: false,
                all: false,
                use_cwd: true,
                warnings_as_errors: false,
                stream_override: None,
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            // Should select root project
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].metadata.name, "root");
        }
    }
}
