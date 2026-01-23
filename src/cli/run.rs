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
#[derive(Debug, Clone)]
pub struct RunArgs {
    /// Target to run (e.g., "test", "build", "lint")
    pub target: String,
    /// Explicit projects to run on (//path/to/project)
    pub projects: Vec<String>,
    /// Skip dependencies (run only specified projects)
    pub no_deps: bool,
    /// Include projects that depend on selected projects
    pub dependents: bool,
    /// Run on all projects
    pub all: bool,
    /// Use current directory to find project (triggered by "." argument)
    pub use_cwd: bool,
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
/// Examples:
///   ["test"]                     -> target=test, all projects
///   ["test", "//a", "//b"]       -> target=test, projects a and b (+ deps)
///   ["test", "--all"]            -> target=test, all projects
///   ["test", "//a", "--no-deps"] -> target=test, only project a
///   ["test", "//a", "--dependents"] -> target=test, a + its dependents
pub fn parse_run_args(args: Vec<String>) -> RunArgs {
    let mut target = String::new();
    let mut projects = Vec::new();
    let mut no_deps = false;
    let mut dependents = false;
    let mut all = false;
    let mut use_cwd = false;

    for arg in args {
        match arg.as_str() {
            "--no-deps" => no_deps = true,
            "--dependents" => dependents = true,
            "--all" => all = true,
            "." => {
                // Current directory - use cwd detection
                use_cwd = true;
            }
            _ if arg.starts_with("--") => {
                // Unknown flag - ignore for now
            }
            _ if arg.starts_with("//") => {
                // Project address
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
        no_deps,
        dependents,
        all,
        use_cwd,
    }
}

/// Select initial projects based on args
///
/// Priority:
/// 1. If --all: return all projects
/// 2. If explicit projects given: return those
/// 3. Otherwise: try to find project in cwd
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

    if args.all {
        // Return all projects
        return Ok(discovered.iter().collect());
    }

    if !args.projects.is_empty() {
        // Return explicitly specified projects
        let mut result = Vec::new();
        for addr in &args.projects {
            if let Some(project) = project_by_addr.get(addr) {
                result.push(*project);
            } else if graph.get(addr).is_some() {
                // Address exists in graph but not in our map - find by address
                for p in discovered {
                    if format!("//{}", p.relative_path.display()) == *addr {
                        result.push(p);
                        break;
                    }
                }
            } else {
                return Err(format!("Project not found: {addr}"));
            }
        }
        return Ok(result);
    }

    // Try to detect project from cwd
    if let Ok(relative_cwd) = cwd.strip_prefix(workspace_root) {
        // Find the most specific project whose root matches or contains cwd
        // (prefer longer paths to avoid matching workspace root for everything)
        let mut best_match: Option<&DiscoveredProject> = None;
        let mut best_match_len = 0;

        for project in discovered {
            let proj_path = &project.relative_path;
            // Check if cwd is within this project's directory
            if relative_cwd.starts_with(proj_path) {
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

    // No projects selected - return error or empty?
    // For usability, if no projects and no --all, show an error
    Err(
        "No projects specified. Use --all to run on all projects, or specify project addresses."
            .to_string(),
    )
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
}
