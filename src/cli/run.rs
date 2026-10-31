//! Run command argument parsing and project selection
//!
//! Handles parsing external subcommand args for target execution
//! and selecting projects based on flags (--all, --no-deps, --dependents).

use std::collections::HashSet;
use std::path::{Component, Path};

use crate::address::ProjectSelector;
use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;

/// Reserved command names that cannot be used as targets
pub const RESERVED_COMMANDS: &[&str] =
    &["list", "graph", "why", "init", "affected", "logs", "help"];

/// Flag spellings `parse_run_args` accepts. Target-command usage must mention each one.
pub const PARSE_RUN_ARG_FLAGS: &[&str] = &[
    "--no-deps",
    "--dependents",
    "--all",
    "--warnings-as-errors",
    "--stream",
    "--no-stream",
    "--jobs",
    "--jobs=",
    "--no-cache",
    "--verbose",
    "-v",
    "--quiet",
    "-q",
    "--json",
    "--full-logs",
    "--lang",
    "--lang=",
];

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
    /// Host execution capacity, validated by the CLI before running targets.
    pub jobs: Option<String>,
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
    /// Filter by source language (e.g., "nodejs", "python", "java", "kotlin")
    pub lang: Vec<String>,
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
/// Args format: `[target] [projects...] [--no-deps] [--dependents] [--all]`
///
/// Project patterns:
///   //path/to/project  - exact project match (`//path/` is the same address)
///   //path/prefix/...  - all projects under prefix
///   //...              - all projects (same as --all)
///   ./...              - all projects under current directory
///   ../web, services/api, services/* - cwd-relative path or glob
///   .                  - project containing the current directory
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
    let mut lang = Vec::new();
    let mut expecting_lang = false;
    let mut jobs = None;
    let mut expecting_jobs = false;

    for arg in args {
        if expecting_jobs {
            expecting_jobs = false;
            jobs = Some(arg);
            continue;
        }
        if expecting_lang {
            expecting_lang = false;
            for l in arg.split(',') {
                let l = l.trim();
                if !l.is_empty() {
                    lang.push(l.to_string());
                }
            }
            continue;
        }

        match arg.as_str() {
            "--no-deps" => no_deps = true,
            "--dependents" => dependents = true,
            "--all" => all = true,
            "--warnings-as-errors" => warnings_as_errors = true,
            "--stream" => stream_override = Some(true),
            "--no-stream" => stream_override = Some(false),
            // Global flags that clap can't parse from external subcommands
            "--jobs" => {
                jobs = Some(String::new());
                expecting_jobs = true;
            }
            _ if arg.starts_with("--jobs=") => jobs = Some(arg["--jobs=".len()..].into()),
            "--no-cache" => no_cache = true,
            "--verbose" | "-v" => verbose = true,
            "--quiet" | "-q" => quiet = true,
            "--json" => json = true,
            "--full-logs" => full_logs = true,
            _ if arg == "--lang" => {
                expecting_lang = true;
            }
            _ if arg.starts_with("--lang=") => {
                let value = &arg["--lang=".len()..];
                for l in value.split(',') {
                    let l = l.trim();
                    if !l.is_empty() {
                        lang.push(l.to_string());
                    }
                }
            }
            "." => {
                // Current directory - the project containing cwd, not a path.
                use_cwd = true;
            }
            _ if arg.starts_with("--") => {
                // Unknown flag - ignore for now
            }
            _ if arg.starts_with("-//") => {
                // Exclusion pattern (e.g., -//vendor/...)
                exclusions.push(strip_trailing_address_slash(&arg[1..]));
            }
            _ if arg.starts_with("//") => {
                // Project address or glob. A trailing slash is the same address.
                projects.push(strip_trailing_address_slash(&arg));
            }
            _ if arg.starts_with("./") || arg.starts_with("../") => {
                // Relative path or glob (./..., ../web, ../...)
                projects.push(arg);
            }
            _ if target.is_empty() => {
                // First positional arg is target
                target = arg;
            }
            _ => {
                // Plain relative selector (services/api, services/*, ../web without a slash form)
                projects.push(arg);
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
        jobs,
        no_cache,
        verbose,
        quiet,
        json,
        full_logs,
        lang,
    }
}

/// `//services/api/` is the same address as `//services/api`. Leave `//` alone.
fn strip_trailing_address_slash(address: &str) -> String {
    let mut trimmed = address.to_string();
    while trimmed.len() > 2 && trimmed.ends_with('/') {
        trimmed.pop();
    }
    trimmed
}

fn unrecognised_selector(selector: &str) -> String {
    format!("unrecognised project selector '{selector}'; use //path, //path/..., ./..., or .")
}

fn outside_workspace(selector: &str, workspace_root: &Path) -> String {
    format!(
        "selector {selector} resolves outside the workspace {}",
        workspace_root.display()
    )
}

fn project_address(project: &DiscoveredProject) -> String {
    format!("//{}", project.relative_path.display())
}

fn address_relative(project_addr: &str) -> String {
    project_addr.trim_start_matches("//").replace('\\', "/")
}

/// `.` and `./` name the project containing cwd, not a path to resolve.
fn is_cwd_dot(selector: &str) -> bool {
    matches!(selector, "." | "./" | "./." | "././")
}

fn logical_normalize(path: &Path) -> Option<std::path::PathBuf> {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(segment) => out.push(segment),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Collapse `.`/`..` in `path_part` against `cwd`, then require the result to
/// stay inside `workspace_root`. The returned path uses `/` separators and is
/// empty at the workspace root.
fn resolve_workspace_relative(
    original: &str,
    path_part: &str,
    cwd: &Path,
    workspace_root: &Path,
) -> Result<String, String> {
    let joined = if path_part.is_empty() || path_part == "." {
        cwd.to_path_buf()
    } else {
        cwd.join(path_part)
    };
    let normalized =
        logical_normalize(&joined).ok_or_else(|| outside_workspace(original, workspace_root))?;
    let relative = normalized
        .strip_prefix(workspace_root)
        .map_err(|_| outside_workspace(original, workspace_root))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

#[derive(Clone)]
enum GlobSegment {
    Literal(String),
    /// Exactly one path segment.
    Star,
    /// Zero or more path segments.
    DoubleStar,
}

/// `*` is one segment and `**` is any depth. Any other glob metacharacter is rejected.
fn parse_glob(pattern: &str) -> Option<Vec<GlobSegment>> {
    let mut segments = Vec::new();
    for segment in pattern.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        if segment == "*" {
            segments.push(GlobSegment::Star);
        } else if segment == "**" {
            segments.push(GlobSegment::DoubleStar);
        } else if segment.contains(['*', '?', '[']) {
            return None;
        } else {
            segments.push(GlobSegment::Literal(segment.to_string()));
        }
    }
    Some(segments)
}

fn glob_matches(pattern: &[GlobSegment], path: &[&str]) -> bool {
    fn rec(pattern: &[GlobSegment], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((GlobSegment::Literal(literal), rest)) => {
                path.first().is_some_and(|segment| segment == literal) && rec(rest, &path[1..])
            }
            Some((GlobSegment::Star, rest)) => !path.is_empty() && rec(rest, &path[1..]),
            Some((GlobSegment::DoubleStar, rest)) => {
                (0..=path.len()).any(|index| rec(rest, &path[index..]))
            }
        }
    }
    rec(pattern, path)
}

enum RelativeSelector {
    Exact(String),
    Recursive(String),
    Glob(Vec<GlobSegment>),
}

fn classify_relative(
    selector: &str,
    cwd: &Path,
    workspace_root: &Path,
) -> Result<RelativeSelector, String> {
    // `../...` and `services/...` are recursive prefixes, not a directory named `...`.
    if !selector.starts_with('/') {
        if let Some(prefix) = selector.strip_suffix("/...") {
            if prefix.contains(['*', '?', '[']) {
                return Err(unrecognised_selector(selector));
            }
            let relative = resolve_workspace_relative(selector, prefix, cwd, workspace_root)?;
            return Ok(RelativeSelector::Recursive(relative));
        }
    }

    let path_part = selector.trim_end_matches('/');
    // Collapse `.`/`..` before reading glob metacharacters so `../*` is
    // `services/*`, and a pattern that leaves the workspace is an outside error.
    let relative = resolve_workspace_relative(selector, path_part, cwd, workspace_root)?;
    if relative.contains(['*', '?', '[']) {
        let Some(glob) = parse_glob(&relative) else {
            return Err(unrecognised_selector(selector));
        };
        return Ok(RelativeSelector::Glob(glob));
    }

    Ok(RelativeSelector::Exact(relative))
}

fn relative_matches(selector: &RelativeSelector, project_rel: &str) -> bool {
    match selector {
        RelativeSelector::Exact(relative) => project_rel == relative,
        RelativeSelector::Recursive(relative) => {
            relative.is_empty()
                || project_rel == relative
                || project_rel.starts_with(&format!("{relative}/"))
        }
        RelativeSelector::Glob(glob) => {
            let path: Vec<&str> = project_rel
                .split('/')
                .filter(|segment| !segment.is_empty())
                .collect();
            glob_matches(glob, &path)
        }
    }
}

fn address_matches(pattern: &str, project_addr: &str) -> bool {
    ProjectSelector::parse(pattern)
        .map(|selector| selector.matches(project_addr))
        .unwrap_or(false)
}

enum PreparedSelector {
    All,
    Cwd,
    Address(String),
    Relative(RelativeSelector),
}

fn prepare_selector(
    selector: &str,
    cwd: &Path,
    workspace_root: &Path,
) -> Result<PreparedSelector, String> {
    if is_cwd_dot(selector) {
        return Ok(PreparedSelector::Cwd);
    }
    if selector.starts_with("//") {
        let address = strip_trailing_address_slash(selector);
        if address == "//..." {
            return Ok(PreparedSelector::All);
        }
        return Ok(PreparedSelector::Address(address));
    }
    Ok(PreparedSelector::Relative(classify_relative(
        selector,
        cwd,
        workspace_root,
    )?))
}

fn selector_matches_address(prepared: &PreparedSelector, project_addr: &str) -> bool {
    match prepared {
        PreparedSelector::All => true,
        PreparedSelector::Cwd => false,
        PreparedSelector::Address(address) => address_matches(address, project_addr),
        PreparedSelector::Relative(selector) => {
            relative_matches(selector, &address_relative(project_addr))
        }
    }
}

/// Project containing `cwd`: the longest discovered directory that is a prefix of cwd.
/// A root project (empty path) matches only when cwd is the workspace root.
fn select_containing_project<'a>(
    discovered: &'a [DiscoveredProject],
    cwd: &Path,
    workspace_root: &Path,
) -> Result<Vec<&'a DiscoveredProject>, String> {
    let no_project = || {
        "No projects specified. Use --all to run on all projects, or specify project addresses."
            .to_string()
    };
    let Some(rel_cwd) = cwd.strip_prefix(workspace_root).ok() else {
        return Err(no_project());
    };

    let mut best_match: Option<&DiscoveredProject> = None;
    let mut best_match_len = 0;
    for project in discovered {
        let proj_path = &project.relative_path;
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

    best_match
        .map(|project| vec![project])
        .ok_or_else(no_project)
}

/// Select initial projects based on args
///
/// Priority:
/// 1. If --all or //...: return all projects (minus exclusions)
/// 2. If explicit projects given: return those (supports glob syntax)
/// 3. Otherwise: try to find project in cwd
///
/// Glob syntax:
/// - `//path/to/project` - exact match (`//path/` is the same address)
/// - `//path/prefix/...` - all projects under that path prefix
/// - `//...` - all projects
/// - `./...`, `../...`, `path/...` - all projects under a cwd-relative prefix
/// - `services/api`, `../web`, `./path` - one project, resolved against cwd
/// - `*` matches one path segment; `**` matches any depth
/// - `.` - the project containing cwd
/// - `-//path/...` - exclude from selection (in args.exclusions)
///
/// A relative selector that matches nothing is an error. The cwd fallback runs
/// only when no selector was given.
pub fn select_projects<'a>(
    args: &RunArgs,
    graph: &'a ProjectGraph,
    discovered: &'a [DiscoveredProject],
    cwd: &Path,
    workspace_root: &Path,
) -> Result<Vec<&'a DiscoveredProject>, String> {
    let project_by_addr: std::collections::HashMap<String, &DiscoveredProject> =
        discovered.iter().map(|p| (project_address(p), p)).collect();

    let select_all = args.all
        || args
            .projects
            .iter()
            .any(|pattern| strip_trailing_address_slash(pattern) == "//...");

    let mut result: Vec<&DiscoveredProject>;
    let mut seen = HashSet::new();

    if select_all {
        result = discovered.iter().collect();
        for project in &result {
            seen.insert(project_address(project));
        }
    } else if args.projects.is_empty() {
        // No selector: the project containing cwd. `.` sets use_cwd and leaves
        // projects empty, so it takes this same path.
        return select_containing_project(discovered, cwd, workspace_root);
    } else {
        result = Vec::new();
        if args.use_cwd {
            for project in select_containing_project(discovered, cwd, workspace_root)? {
                if seen.insert(project_address(project)) {
                    result.push(project);
                }
            }
        }

        for pattern in &args.projects {
            let prepared = prepare_selector(pattern, cwd, workspace_root)?;
            if matches!(prepared, PreparedSelector::All) {
                continue;
            }
            if matches!(prepared, PreparedSelector::Cwd) {
                for project in select_containing_project(discovered, cwd, workspace_root)? {
                    if seen.insert(project_address(project)) {
                        result.push(project);
                    }
                }
                continue;
            }

            let mut found_any = false;
            for project in discovered {
                let project_addr = project_address(project);
                if selector_matches_address(&prepared, &project_addr) {
                    found_any = true;
                    if seen.insert(project_addr) {
                        result.push(project);
                    }
                }
            }

            if found_any {
                continue;
            }

            if let PreparedSelector::Address(address) = &prepared {
                if address.ends_with("/...") {
                    return Err(format!("No projects found matching: {address}"));
                }
                if graph.get(address).is_none() && !project_by_addr.contains_key(address) {
                    return Err(format!("Project not found: {address}"));
                }
            } else {
                return Err(unrecognised_selector(pattern));
            }
        }
    }

    if !args.exclusions.is_empty() {
        result.retain(|project| {
            let project_addr = project_address(project);
            !args.exclusions.iter().any(|exclusion| {
                let exclusion = strip_trailing_address_slash(exclusion);
                if exclusion.starts_with("//") {
                    address_matches(&exclusion, &project_addr)
                } else {
                    prepare_selector(&exclusion, cwd, workspace_root)
                        .ok()
                        .is_some_and(|prepared| selector_matches_address(&prepared, &project_addr))
                }
            })
        });
    }

    if !args.lang.is_empty() {
        result.retain(|project| project.has_any_language(&args.lang));
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
    fn target_command_usage_lists_every_parse_run_args_flag() {
        let usage = crate::cli::commands::TARGET_COMMAND_USAGE;
        for flag in PARSE_RUN_ARG_FLAGS {
            assert!(usage.contains(flag), "target command usage missing {flag}");
        }
    }

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
        // `.` is the cwd selector (project containing cwd), not a stored path.
        let args = parse_run_args(vec!["test".to_string(), ".".to_string()]);

        assert_eq!(args.target, "test");
        assert!(args.projects.is_empty());
        assert!(args.use_cwd);
    }

    #[test]
    fn test_parse_run_args_keeps_relative_selectors() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "services/api".to_string(),
            "../web".to_string(),
            "services/*".to_string(),
            "services/**".to_string(),
            "./...".to_string(),
            "../...".to_string(),
            "--no-deps".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert_eq!(
            args.projects,
            vec![
                "services/api",
                "../web",
                "services/*",
                "services/**",
                "./...",
                "../..."
            ]
        );
        assert!(args.no_deps);
        assert!(!args.use_cwd);
    }

    #[test]
    fn test_parse_run_args_strips_trailing_address_slash() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "//services/api/".to_string(),
            "-//services/web/".to_string(),
        ]);

        assert_eq!(args.projects, vec!["//services/api"]);
        assert_eq!(args.exclusions, vec!["//services/web"]);
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

    #[test]
    fn test_parse_run_args_with_lang_flag() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "--all".to_string(),
            "--lang".to_string(),
            "nodejs".to_string(),
        ]);

        assert_eq!(args.target, "test");
        assert!(args.all);
        assert_eq!(args.lang, vec!["nodejs"]);
    }

    #[test]
    fn test_parse_run_args_with_lang_comma_separated() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "--all".to_string(),
            "--lang".to_string(),
            "nodejs,python".to_string(),
        ]);

        assert_eq!(args.lang, vec!["nodejs", "python"]);
    }

    #[test]
    fn test_parse_run_args_with_lang_equals() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "--all".to_string(),
            "--lang=rust,go".to_string(),
        ]);

        assert_eq!(args.lang, vec!["rust", "go"]);
    }

    #[test]
    fn test_parse_run_args_with_multiple_lang_flags() {
        let args = parse_run_args(vec![
            "test".to_string(),
            "--all".to_string(),
            "--lang".to_string(),
            "nodejs".to_string(),
            "--lang".to_string(),
            "python".to_string(),
        ]);

        assert_eq!(args.lang, vec!["nodejs", "python"]);
    }

    #[test]
    fn test_parse_run_args_lang_default_empty() {
        let args = parse_run_args(vec!["test".to_string()]);

        assert!(args.lang.is_empty());
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
                languages: vec!["nodejs".to_string()],
                build_system: None,
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

        fn make_project_with_lang(
            name: &str,
            relative_path: &str,
            plugin: &str,
        ) -> DiscoveredProject {
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
                plugin_name: plugin.to_string(),
                languages: vec![plugin.to_string()],
                build_system: None,
                relative_path: PathBuf::from(relative_path),
            }
        }

        #[test]
        fn test_select_projects_lang_filter() {
            let mut mixed_jvm = make_project_with_lang("mixed-jvm", "src/mixed", "gradle");
            mixed_jvm.languages = vec!["java".to_string(), "kotlin".to_string()];
            mixed_jvm.build_system = Some("gradle".to_string());
            let projects = vec![
                make_project_with_lang("node-app", "src/node-app", "nodejs"),
                make_project_with_lang("py-app", "src/py-app", "python"),
                make_project_with_lang("rust-lib", "src/rust-lib", "rust"),
                mixed_jvm,
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                all: true,
                lang: vec!["nodejs".to_string(), "kotlin".to_string()],
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
            let names: Vec<&str> = selected.iter().map(|p| p.metadata.name.as_str()).collect();
            assert!(names.contains(&"node-app"));
            assert!(names.contains(&"mixed-jvm"));
            assert!(!names.contains(&"py-app"));
            assert!(!names.contains(&"rust-lib"));
        }

        #[test]
        fn test_select_projects_lang_filter_empty_means_no_filter() {
            let projects = vec![
                make_project_with_lang("node-app", "src/node-app", "nodejs"),
                make_project_with_lang("py-app", "src/py-app", "python"),
            ];
            let graph = build_graph(&projects).unwrap();
            let workspace_root = Path::new("/workspace");
            let cwd = Path::new("/workspace");

            let args = RunArgs {
                target: "test".to_string(),
                all: true,
                lang: vec![],
                ..Default::default()
            };

            let selected = select_projects(&args, &graph, &projects, cwd, workspace_root).unwrap();

            assert_eq!(selected.len(), 2);
        }

        fn service_projects() -> Vec<DiscoveredProject> {
            vec![
                make_project("api", "services/api"),
                make_project("web", "services/web"),
                make_project("api-v2", "services/api/v2"),
                make_project("core", "libs/core"),
            ]
        }

        fn selected_names(
            projects: &[DiscoveredProject],
            selectors: &[&str],
            cwd: &Path,
        ) -> Result<Vec<String>, String> {
            let graph = build_graph(projects).unwrap();
            let args = RunArgs {
                target: "test".to_string(),
                projects: selectors
                    .iter()
                    .map(|selector| (*selector).to_string())
                    .collect(),
                ..Default::default()
            };
            let selected = select_projects(&args, &graph, projects, cwd, Path::new("/workspace"))?;
            Ok(selected
                .iter()
                .map(|project| project.metadata.name.clone())
                .collect())
        }

        #[test]
        fn test_select_projects_relative_directory_and_globs() {
            let projects = service_projects();
            let root = Path::new("/workspace");

            let api = selected_names(&projects, &["services/api"], root).unwrap();
            assert_eq!(api, vec!["api".to_string()]);

            let star = selected_names(&projects, &["services/*"], root).unwrap();
            assert!(star.contains(&"api".to_string()));
            assert!(star.contains(&"web".to_string()));
            assert!(!star.contains(&"api-v2".to_string()));
            assert!(!star.contains(&"core".to_string()));

            let deep = selected_names(&projects, &["services/**"], root).unwrap();
            assert!(deep.contains(&"api".to_string()));
            assert!(deep.contains(&"web".to_string()));
            assert!(deep.contains(&"api-v2".to_string()));
            assert!(!deep.contains(&"core".to_string()));

            let under_cwd = selected_names(&projects, &["./..."], root).unwrap();
            assert_eq!(under_cwd.len(), 4);
        }

        #[test]
        fn test_select_projects_parent_selectors_resolve_against_cwd() {
            let projects = service_projects();
            let web = Path::new("/workspace/services/web");

            let api = selected_names(&projects, &["../api"], web).unwrap();
            assert_eq!(api, vec!["api".to_string()]);

            let under_services = selected_names(&projects, &["../..."], web).unwrap();
            assert!(under_services.contains(&"api".to_string()));
            assert!(under_services.contains(&"web".to_string()));
            assert!(under_services.contains(&"api-v2".to_string()));
            assert!(!under_services.contains(&"core".to_string()));

            let star = selected_names(&projects, &["../*"], web).unwrap();
            assert!(star.contains(&"api".to_string()));
            assert!(star.contains(&"web".to_string()));
            assert!(!star.contains(&"api-v2".to_string()));
            assert!(!star.contains(&"core".to_string()));
        }

        #[test]
        fn test_select_projects_unknown_selector_does_not_fall_back_to_cwd() {
            let projects = service_projects();
            let api = Path::new("/workspace/services/api");
            let error = selected_names(&projects, &["bogus"], api).unwrap_err();
            assert_eq!(
                error,
                "unrecognised project selector 'bogus'; use //path, //path/..., ./..., or ."
            );
        }

        #[test]
        fn test_select_projects_outside_workspace_names_selector_and_root() {
            let projects = service_projects();
            let error = selected_names(&projects, &[".."], Path::new("/workspace")).unwrap_err();
            assert_eq!(
                error,
                "selector .. resolves outside the workspace /workspace"
            );
            let glob = selected_names(&projects, &["../*"], Path::new("/workspace")).unwrap_err();
            assert_eq!(
                glob,
                "selector ../* resolves outside the workspace /workspace"
            );
        }

        #[test]
        fn test_select_projects_trailing_slash_address_matches_exact_project() {
            let projects = service_projects();
            let selected =
                selected_names(&projects, &["//services/api/"], Path::new("/workspace")).unwrap();
            assert_eq!(selected, vec!["api".to_string()]);
        }

        #[test]
        fn test_select_projects_unsupported_glob_is_unrecognised() {
            let projects = service_projects();
            let error =
                selected_names(&projects, &["services/api*"], Path::new("/workspace")).unwrap_err();
            assert!(
                error.starts_with("unrecognised project selector 'services/api*'"),
                "{error}"
            );
        }
    }
}
