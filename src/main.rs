//! Aster CLI entry point
//!
//! Build orchestration for polyglot monorepos.

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use std::env;
use std::fs;
use std::process::ExitCode;

use aster::cli::{
    build_execution_output, check_reserved_target, expand_selection, output_json, parse_run_args,
    print_summary, select_projects, Cli, Commands, GraphOutput, OutputMode, ProjectCommands,
    ProjectInfo, WhyOutput,
};
use aster::config::find_workspace_root;
use aster::discovery::{discover_projects, DiscoveredProject};
use aster::executor::logs::LogStore;
use aster::executor::Executor;
use aster::git::{affected_with_dependents, files_to_projects, AffectedDetector};
use aster::graph::{build_graph, build_target_graph, find_cycle, format_path, TargetGraph};
use aster::plugins::{
    ElixirPlugin, GoPlugin, NodeJsPlugin, PluginRegistry, PythonPlugin, Target, TargetCapability,
};
use chrono::{DateTime, Utc};
use globset::{Glob, GlobMatcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Number of characters to show when displaying truncated cache hashes
const CACHE_HASH_DISPLAY_LENGTH: usize = 8;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let output_mode = cli.output_mode();
    let full_logs = cli.full_logs();

    let cwd = env::current_dir().context("Failed to get current directory")?;

    // Handle init command specially - it works even without an existing workspace
    if matches!(cli.command, Commands::Init) {
        return handle_init(&cwd, cli.verbose);
    }

    // For all other commands, require a workspace
    let workspace_root = find_workspace_root(&cwd).context(
        "Not in an aster workspace (no aster.toml or .git found). Run 'aster init' to create one.",
    )?;

    if output_mode == OutputMode::Verbose {
        eprintln!("[aster] Workspace root: {}", workspace_root.display());
    }

    // Set up plugin registry with all language plugins
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(NodeJsPlugin));
    registry.register(Box::new(ElixirPlugin));
    registry.register(Box::new(PythonPlugin));
    registry.register(Box::new(GoPlugin));

    // Discover projects
    let projects =
        discover_projects(&workspace_root, &registry).context("Failed to discover projects")?;

    if output_mode == OutputMode::Verbose {
        eprintln!("[aster] Discovered {} projects", projects.len());
    }

    match cli.command {
        Commands::Init => unreachable!("Init handled above"),
        Commands::List { path } => {
            // Filter projects by path if specified
            let filtered_projects: Vec<&DiscoveredProject> = if let Some(ref filter_path) = path {
                // Resolve the filter path relative to cwd
                let filter_path = if filter_path == "." {
                    // Current directory relative to workspace root
                    cwd.strip_prefix(&workspace_root)
                        .map(|p| p.to_path_buf())
                        .unwrap_or_default()
                } else {
                    // Normalize the path (remove trailing slashes, handle ./)
                    let p = filter_path.trim_end_matches('/');
                    let p = p.strip_prefix("./").unwrap_or(p);
                    PathBuf::from(p)
                };

                projects
                    .iter()
                    .filter(|p| p.relative_path.starts_with(&filter_path))
                    .collect()
            } else {
                projects.iter().collect()
            };

            if output_mode == OutputMode::Json {
                // JSON output: array of ProjectInfo
                let project_infos: Vec<ProjectInfo> = filtered_projects
                    .iter()
                    .map(|p| {
                        let targets: HashMap<String, String> = p
                            .targets
                            .iter()
                            .map(|(k, v)| (k.clone(), v.command.clone()))
                            .collect();
                        ProjectInfo {
                            address: format!("//{}", p.relative_path.display()),
                            path: p.relative_path.display().to_string(),
                            plugin: p.plugin_name.clone(),
                            targets,
                        }
                    })
                    .collect();
                output_json(&project_infos)?;
            } else if output_mode != OutputMode::Quiet {
                // Normal or Verbose: text output with colors
                for project in &filtered_projects {
                    println!(
                        "{}",
                        style(format!("//{}", project.relative_path.display()))
                            .cyan()
                            .bold()
                    );

                    if !project.targets.is_empty() {
                        // Sort targets for consistent output
                        let mut target_names: Vec<&str> =
                            project.targets.keys().map(|s| s.as_str()).collect();
                        target_names.sort();

                        for name in target_names {
                            let target = &project.targets[name];
                            if target.depends_on.is_empty() {
                                println!("  {}: {}", style(name).yellow(), target.command);
                            } else {
                                println!(
                                    "  {}: {} {} {}",
                                    style(name).yellow(),
                                    target.command,
                                    style("→").dim(),
                                    style(format!("[{}]", target.depends_on.join(", "))).dim()
                                );
                            }
                        }
                    }
                }
            }
            // Quiet mode: no output for list
        }
        Commands::Graph { target } => {
            // Build the target graph
            let graph = build_target_graph(&projects);

            // Check for cycles
            if let Some(cycle) = graph.find_cycle() {
                eprintln!("error: {cycle}");
                return Err(anyhow::anyhow!("Dependency cycle detected"));
            }

            if output_mode == OutputMode::Json {
                // JSON output: nodes and edges
                let (nodes, edges) = if let Some(ref addr) = target {
                    // Specific target: show its subgraph (target + dependencies)
                    if graph.get(addr).is_none() {
                        return Err(anyhow::anyhow!("Target not found: {addr}"));
                    }

                    let mut nodes = vec![addr.clone()];
                    let mut edges: HashMap<String, Vec<String>> = HashMap::new();

                    let deps = graph.dependencies(addr);
                    let dep_addrs: Vec<String> = deps.iter().map(|d| d.address.clone()).collect();

                    if !dep_addrs.is_empty() {
                        edges.insert(addr.clone(), dep_addrs.clone());
                        nodes.extend(dep_addrs);
                    } else {
                        edges.insert(addr.clone(), vec![]);
                    }

                    (nodes, edges)
                } else {
                    // Full graph
                    let nodes: Vec<String> = graph.targets().map(|t| t.address.clone()).collect();
                    let mut edges: HashMap<String, Vec<String>> = HashMap::new();

                    for node in graph.targets() {
                        let deps = graph.dependencies(&node.address);
                        let dep_addrs: Vec<String> =
                            deps.iter().map(|d| d.address.clone()).collect();
                        edges.insert(node.address.clone(), dep_addrs);
                    }

                    (nodes, edges)
                };

                let output = GraphOutput { nodes, edges };
                output_json(&output)?;
            } else if output_mode == OutputMode::Quiet {
                // Quiet mode: no output for graph
            } else {
                // Normal/Verbose: text output with colors
                if let Some(addr) = target {
                    // Show deps for specific target
                    if let Some(node) = graph.get(&addr) {
                        println!("{}", style(&node.address).cyan().bold());
                        for dep in graph.dependencies(&addr) {
                            println!("  {} {}", style("→").dim(), style(&dep.address).dim());
                        }
                    } else {
                        return Err(anyhow::anyhow!("Target not found: {addr}"));
                    }
                } else {
                    // Show full graph grouped by project
                    let mut current_project = String::new();
                    let mut targets: Vec<_> = graph.targets().collect();
                    targets.sort_by(|a, b| a.address.cmp(&b.address));

                    for node in targets {
                        if node.project_address != current_project {
                            if !current_project.is_empty() {
                                println!();
                            }
                            current_project = node.project_address.clone();
                            println!("{}", style(&current_project).cyan().bold());
                        }
                        let deps = graph.dependencies(&node.address);
                        if deps.is_empty() {
                            println!("  {}", style(format!(":{}", node.target_name)).yellow());
                        } else {
                            let dep_strs: Vec<&str> =
                                deps.iter().map(|d| d.address.as_str()).collect();
                            println!(
                                "  {} {} {}",
                                style(format!(":{}", node.target_name)).yellow(),
                                style("→").dim(),
                                style(format!("[{}]", dep_strs.join(", "))).dim()
                            );
                        }
                    }
                }
            }
        }
        Commands::Why { from, to } => {
            // Build target graph for path finding
            let graph = build_target_graph(&projects);

            // Validate targets exist
            if graph.get(&from).is_none() {
                return Err(anyhow::anyhow!("Target not found: {from}"));
            }
            if graph.get(&to).is_none() {
                return Err(anyhow::anyhow!("Target not found: {to}"));
            }

            // Find path
            let path = graph.find_path(&from, &to);

            if output_mode == OutputMode::Json {
                let output = WhyOutput {
                    from: from.clone(),
                    to: to.clone(),
                    path,
                };
                output_json(&output)?;
            } else if output_mode != OutputMode::Quiet {
                // Normal/Verbose: text output
                match path {
                    Some(p) => {
                        println!("{}", format_path(&p));
                    }
                    None => {
                        println!("No dependency path found between {from} and {to}");
                    }
                }
            }
            // Quiet mode: no output for why
        }
        Commands::Logs { target } => {
            let log_store = LogStore::new(&workspace_root);

            if let Some(ref target_addr) = target {
                // Specific target: show full output
                // Support both "//project:target" and "//project target" formats
                let normalized_addr = if target_addr.contains(':') {
                    target_addr.clone()
                } else if target_addr.contains(' ') {
                    // "//project target" format - convert to "//project:target"
                    let parts: Vec<&str> = target_addr.splitn(2, ' ').collect();
                    if parts.len() == 2 {
                        format!("{}:{}", parts[0], parts[1])
                    } else {
                        target_addr.clone()
                    }
                } else {
                    target_addr.clone()
                };

                match log_store.get_target_log(&normalized_addr)? {
                    Some(target_log) => {
                        if output_mode == OutputMode::Json {
                            output_json(&target_log)?;
                        } else {
                            println!("--- {} ---", target_log.address);
                            if !target_log.output.is_empty() {
                                print!("{}", target_log.output);
                                if !target_log.output.ends_with('\n') {
                                    println!();
                                }
                            }
                            println!(
                                "[{}] {} ({}ms)",
                                target_log.status.to_uppercase(),
                                target_log.address,
                                target_log.duration_ms
                            );
                        }
                    }
                    None => {
                        // Exit silently for missing target (per CONTEXT.md - not an error)
                        if output_mode == OutputMode::Json {
                            // Output empty object for consistency
                            println!("{{}}");
                        }
                    }
                }
            } else {
                // No target: show summary of last run
                match log_store.load_latest()? {
                    Some(run) => {
                        if output_mode == OutputMode::Json {
                            output_json(&run)?;
                        } else {
                            use aster::ui::colors::{status_fail, status_pass, status_skip};

                            println!("Last run: {} ({})", run.target, run.timestamp);
                            println!();
                            for result in &run.results {
                                let status_icon = match result.status.as_str() {
                                    "passed" => status_pass(),
                                    "failed" => status_fail(),
                                    _ => status_skip(),
                                };
                                println!("  {} {}", status_icon, result.address);
                            }
                            println!();
                            println!("Use `aster logs <target>` to view full output");
                        }
                    }
                    None => {
                        if output_mode == OutputMode::Json {
                            // Output empty object for no previous run
                            println!("{{}}");
                        } else {
                            println!("No previous run found");
                        }
                    }
                }
            }
        }
        Commands::Affected {
            target,
            base,
            head,
            dependents,
            dry_run,
            only_affected_files,
            warnings_as_errors,
        } => {
            // Create affected detector from workspace (requires git)
            let detector = AffectedDetector::new(&workspace_root)
                .context("Not in a git repository. The 'affected' command requires git.")?;

            // Get changed files
            let changed_files = detector
                .all_affected_files(&base, head.as_deref())
                .with_context(|| {
                    format!(
                        "Failed to detect changed files between '{}' and '{}'",
                        base,
                        head.as_deref().unwrap_or("HEAD + uncommitted")
                    )
                })?;

            if output_mode == OutputMode::Verbose {
                eprintln!("[aster] Found {} changed files", changed_files.len());
                for file in &changed_files {
                    eprintln!("  - {}", file.display());
                }
            }

            // Map files to projects
            let directly_affected = files_to_projects(&changed_files, &projects);

            if output_mode == OutputMode::Verbose {
                eprintln!("[aster] Directly affected: {directly_affected:?}");
            }

            // Build the graph
            let graph = build_graph(&projects)?;

            // Check for cycles
            if let Some(cycle) = find_cycle(&graph) {
                eprintln!("error: {cycle}");
                return Err(anyhow::anyhow!("Dependency cycle detected"));
            }

            // Expand with dependents if requested
            let affected_addrs = if dependents {
                affected_with_dependents(directly_affected, &graph)
            } else {
                directly_affected
            };

            // Find DiscoveredProject refs for affected addresses
            let affected_projects: Vec<_> = projects
                .iter()
                .filter(|p| {
                    let addr = format!("//{}", p.relative_path.display());
                    affected_addrs.contains(&addr)
                })
                .collect();

            if affected_projects.is_empty() {
                if output_mode == OutputMode::Json {
                    let output = build_execution_output(&[]);
                    output_json(&output)?;
                } else if output_mode != OutputMode::Quiet {
                    println!("No projects affected");
                }
                return Ok(());
            }

            // Sort by dependency order
            let ordered = graph.topological_order_subset(&affected_projects);

            if output_mode == OutputMode::Verbose {
                eprintln!(
                    "[aster] Running '{}' on {} affected projects",
                    target,
                    ordered.len()
                );
            }

            // Print affected projects list (unless quiet or json without dry_run)
            if output_mode != OutputMode::Quiet && (output_mode != OutputMode::Json || dry_run) {
                println!(
                    "Affected projects ({}):",
                    if dependents {
                        "including dependents"
                    } else {
                        "directly affected only"
                    }
                );
                for p in &ordered {
                    println!("  //{}", p.relative_path.display());
                }
            }

            // Dry run: just show what would run
            if dry_run {
                // Build map of project -> affected files
                let mut files_per_project: HashMap<String, Vec<String>> = HashMap::new();
                for project in &ordered {
                    let project_addr = format!("//{}", project.relative_path.display());
                    let project_files: Vec<String> = changed_files
                        .iter()
                        .filter(|f| f.starts_with(&project.relative_path))
                        .map(|f| f.to_string_lossy().to_string())
                        .collect();
                    files_per_project.insert(project_addr, project_files);
                }

                if output_mode == OutputMode::Json {
                    // Output JSON list of affected projects with files
                    #[derive(serde::Serialize)]
                    struct AffectedProject {
                        address: String,
                        files: Vec<String>,
                    }
                    #[derive(serde::Serialize)]
                    struct DryRunOutput {
                        target: String,
                        base: String,
                        head: Option<String>,
                        affected: Vec<AffectedProject>,
                        count: usize,
                    }
                    let affected: Vec<AffectedProject> = ordered
                        .iter()
                        .map(|p| {
                            let addr = format!("//{}", p.relative_path.display());
                            let files = files_per_project.get(&addr).cloned().unwrap_or_default();
                            AffectedProject {
                                address: addr,
                                files,
                            }
                        })
                        .collect();
                    let output = DryRunOutput {
                        target: target.clone(),
                        base: base.clone(),
                        head: head.clone(),
                        count: affected.len(),
                        affected,
                    };
                    output_json(&output)?;
                } else if output_mode != OutputMode::Quiet {
                    println!();
                    println!("Would run '{}' on {} projects:", target, ordered.len());
                    println!();
                    for project in &ordered {
                        let addr = format!("//{}", project.relative_path.display());
                        println!("  {addr}");
                        if let Some(files) = files_per_project.get(&addr) {
                            for file in files {
                                println!("    - {file}");
                            }
                        }
                    }
                }
                return Ok(());
            }

            // Execute target on projects
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache);

            // Build command overrides for files-list and warnings-as-errors
            let results = if only_affected_files || warnings_as_errors {
                let mut command_overrides: HashMap<String, String> = HashMap::new();

                // Create plugin registry for capability handling
                let mut registry = PluginRegistry::new();
                registry.register(Box::new(NodeJsPlugin));
                registry.register(Box::new(PythonPlugin));
                registry.register(Box::new(ElixirPlugin));
                registry.register(Box::new(GoPlugin));

                for project in &ordered {
                    let project_addr = format!("//{}", project.relative_path.display());
                    let target_addr = format!("{project_addr}:{target}");

                    if let Some(target_def) = project.targets.get(&target) {
                        let mut modified_cmd: Option<String> = None;

                        // Apply files-list if requested and supported
                        if only_affected_files
                            && target_def
                                .capabilities
                                .contains(&TargetCapability::FilesList)
                        {
                            let project_files: Vec<PathBuf> = changed_files
                                .iter()
                                .filter(|f| f.starts_with(&project.relative_path))
                                .map(|f| {
                                    f.strip_prefix(&project.relative_path)
                                        .unwrap_or(f)
                                        .to_path_buf()
                                })
                                .collect();

                            modified_cmd = apply_files_to_command(
                                target_def,
                                &project_files,
                                &target,
                                &project.plugin_name,
                                &registry,
                            );
                        }

                        // Apply warnings-as-errors if requested and supported
                        if warnings_as_errors
                            && target_def
                                .capabilities
                                .contains(&TargetCapability::WarningsAsErrors)
                        {
                            // Build a temporary target with possibly modified command
                            let cmd_to_modify =
                                modified_cmd.as_ref().unwrap_or(&target_def.command).clone();
                            let temp_target = Target {
                                command: cmd_to_modify,
                                ..target_def.clone()
                            };
                            if let Some(warnings_cmd) = apply_warnings_as_errors(
                                &temp_target,
                                &target,
                                &project.plugin_name,
                                &registry,
                            ) {
                                modified_cmd = Some(warnings_cmd);
                            }
                        }

                        if let Some(cmd) = modified_cmd {
                            command_overrides.insert(target_addr, cmd);
                        }
                    }
                }

                if output_mode == OutputMode::Verbose && !command_overrides.is_empty() {
                    eprintln!(
                        "[aster] Using modified commands for {} targets",
                        command_overrides.len()
                    );
                }

                executor.execute_with_command_overrides(&target, &ordered, &command_overrides, None)
            } else {
                executor.execute(&target, &ordered, &graph, None)
            };

            // Output results based on mode
            if output_mode == OutputMode::Json {
                let output = build_execution_output(&results);
                output_json(&output)?;
            } else {
                print_summary(&results, &target, output_mode, true);
            }

            // Return error if any failed (for exit code)
            let failed = results.iter().filter(|r| !r.success).count();
            if failed > 0 {
                return Err(anyhow::anyhow!("{failed} project(s) failed"));
            }
        }
        Commands::Run { targets, no_deps } => {
            // Heterogeneous run: execute multiple //project:target pairs

            // Validate all targets exist and have correct format
            let target_graph = build_target_graph(&projects);

            // Check for cycles
            if let Some(cycle) = target_graph.find_cycle() {
                eprintln!("error: {cycle}");
                return Err(anyhow::anyhow!("Dependency cycle detected"));
            }

            let mut invalid_targets = Vec::new();
            for target in &targets {
                if !target.contains(':') {
                    invalid_targets.push(format!("{target} (missing :target suffix)"));
                } else if target_graph.get(target).is_none() {
                    invalid_targets.push(format!("{target} (not found)"));
                }
            }

            if !invalid_targets.is_empty() {
                return Err(anyhow::anyhow!(
                    "Invalid targets:\n  {}\n\nFormat: //project:target (e.g., //services/api:test)",
                    invalid_targets.join("\n  ")
                ));
            }

            // Collect all targets to run (and optionally their dependencies)
            let mut targets_to_run: std::collections::HashSet<String> =
                targets.iter().cloned().collect();

            if !no_deps {
                // Add dependencies of each target
                for target in &targets {
                    collect_target_deps_recursive(target, &target_graph, &mut targets_to_run);
                }
            }

            // Build project map for executor
            let project_map: std::collections::HashMap<String, &DiscoveredProject> = projects
                .iter()
                .map(|p| (format!("//{}", p.relative_path.display()), p))
                .collect();

            // Compute DAG levels and execute
            let levels = compute_heterogeneous_levels(&targets_to_run, &project_map);

            if output_mode == OutputMode::Verbose {
                eprintln!(
                    "[aster] Running {} targets across {} levels",
                    targets_to_run.len(),
                    levels.len()
                );
                for (i, level) in levels.iter().enumerate() {
                    eprintln!("[aster] Level {i}: {level:?}");
                }
            }

            // Execute using the executor's infrastructure
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache);
            let results =
                execute_heterogeneous(&executor, &levels, &project_map, output_mode, full_logs);

            // Output results based on mode
            if output_mode == OutputMode::Json {
                let output = build_execution_output(&results);
                output_json(&output)?;
            } else {
                print_heterogeneous_summary(&results, output_mode, full_logs);
            }

            // Return error if any failed (for exit code)
            let failed = results.iter().filter(|r| !r.success && !r.skipped).count();
            if failed > 0 {
                return Err(anyhow::anyhow!("{failed} target(s) failed"));
            }
        }
        Commands::Project { command } => {
            handle_project_command(command, &workspace_root, &projects, output_mode)?;
        }
        Commands::Cache { command } => {
            use aster::cache::CacheStore;
            use aster::cli::CacheCommands;

            let cache_store = CacheStore::new(&workspace_root);

            match command {
                CacheCommands::Clear { target } => {
                    if let Some(pattern) = target {
                        let removed = cache_store.clear_matching(&pattern)?;
                        if output_mode != OutputMode::Quiet {
                            println!("Cleared {removed} cache entries matching {pattern}");
                        }
                    } else {
                        cache_store.clear()?;
                        if output_mode != OutputMode::Quiet {
                            println!("Cache cleared");
                        }
                    }
                }
                CacheCommands::Status { target } => {
                    let state = cache_store.load()?;

                    if let Some(addr) = target {
                        // Show specific target
                        if let Some(entry) = state.targets.get(&addr) {
                            if output_mode == OutputMode::Json {
                                output_json(&entry)?;
                            } else {
                                let hash_display =
                                    &entry.hash[..CACHE_HASH_DISPLAY_LENGTH.min(entry.hash.len())];
                                let time_display = format_relative_time(&entry.timestamp);
                                println!("{addr}: {hash_display} ({time_display})");
                            }
                        } else if output_mode == OutputMode::Json {
                            println!("null");
                        } else {
                            println!("{addr}: not cached");
                        }
                    } else {
                        // Show all
                        if output_mode == OutputMode::Json {
                            output_json(&state)?;
                        } else if state.targets.is_empty() {
                            println!("No cached targets");
                        } else {
                            println!("Cached targets ({}):", state.targets.len());
                            let mut addrs: Vec<_> = state.targets.keys().collect();
                            addrs.sort();
                            for addr in addrs {
                                let entry = &state.targets[addr];
                                let hash_display =
                                    &entry.hash[..CACHE_HASH_DISPLAY_LENGTH.min(entry.hash.len())];
                                let time_display = format_relative_time(&entry.timestamp);
                                println!("  {addr}: {hash_display} ({time_display})");
                            }
                        }
                    }
                }
            }
        }
        Commands::Target(args) => {
            // Parse external subcommand args
            let run_args = parse_run_args(args);

            if run_args.target.is_empty() {
                return Err(anyhow::anyhow!("No target specified. Usage: aster <target> [projects...] [--all] [--no-deps] [--dependents]"));
            }

            // Check for reserved command conflicts
            if let Some(err_msg) = check_reserved_target(&run_args.target) {
                return Err(anyhow::anyhow!("{err_msg}"));
            }

            // Build the graph
            let graph = build_graph(&projects)?;

            // Check for cycles
            if let Some(cycle) = find_cycle(&graph) {
                eprintln!("error: {cycle}");
                return Err(anyhow::anyhow!("Dependency cycle detected"));
            }

            // Select initial projects
            let initial = select_projects(&run_args, &graph, &projects, &cwd, &workspace_root)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            // Build set of primary projects (originally selected, before expansion)
            // Only these will run the requested target; dependency projects are included
            // for target-level dependency resolution but won't run the requested target
            let primary_projects: HashSet<String> = initial
                .iter()
                .map(|p| format!("//{}", p.relative_path.display()))
                .collect();

            // Expand selection based on flags
            let selected = expand_selection(&run_args, &initial, &graph, &projects);

            // Sort by dependency order
            let ordered = graph.topological_order_subset(&selected);

            if output_mode == OutputMode::Verbose {
                eprintln!(
                    "[aster] Running '{}' on {} projects",
                    run_args.target,
                    primary_projects.len()
                );
            }

            // Determine if streaming is enabled for this target
            // CLI override takes precedence, otherwise use target config
            let should_stream = if let Some(override_val) = run_args.stream_override {
                override_val
            } else {
                // Check if the target has stream=true in any primary project
                initial.iter().any(|p| {
                    p.targets
                        .get(&run_args.target)
                        .map(|t| t.stream)
                        .unwrap_or(false)
                })
            };

            // Execute target on projects
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache);

            // Handle streaming execution (single project only, output to terminal)
            if should_stream {
                if initial.len() != 1 {
                    return Err(anyhow::anyhow!(
                        "Streaming mode only supports running on a single project.\n\
                        Found {} projects selected. Use a more specific selector or --no-stream.",
                        initial.len()
                    ));
                }

                let project = initial[0];
                match executor.execute_streaming(&run_args.target, project) {
                    Ok(exit_code) => {
                        if exit_code != 0 {
                            return Err(anyhow::anyhow!("Process exited with code {exit_code}"));
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("{e}"));
                    }
                }
            }

            let results = if run_args.warnings_as_errors {
                // Build command overrides for warnings-as-errors
                let mut command_overrides: HashMap<String, String> = HashMap::new();

                // Create plugin registry for capability handling
                let mut registry = PluginRegistry::new();
                registry.register(Box::new(NodeJsPlugin));
                registry.register(Box::new(PythonPlugin));
                registry.register(Box::new(ElixirPlugin));
                registry.register(Box::new(GoPlugin));

                for project in &ordered {
                    let project_addr = format!("//{}", project.relative_path.display());
                    let target_addr = format!("{project_addr}:{}", run_args.target);

                    if let Some(target_def) = project.targets.get(&run_args.target) {
                        if let Some(modified_cmd) = apply_warnings_as_errors(
                            target_def,
                            &run_args.target,
                            &project.plugin_name,
                            &registry,
                        ) {
                            command_overrides.insert(target_addr, modified_cmd);
                        }
                    }
                }

                if output_mode == OutputMode::Verbose && !command_overrides.is_empty() {
                    eprintln!(
                        "[aster] Using warnings-as-errors for {} targets",
                        command_overrides.len()
                    );
                }

                executor.execute_with_command_overrides(
                    &run_args.target,
                    &ordered,
                    &command_overrides,
                    Some(&primary_projects),
                )
            } else {
                executor.execute(&run_args.target, &ordered, &graph, Some(&primary_projects))
            };

            // Output results based on mode
            if output_mode == OutputMode::Json {
                let output = build_execution_output(&results);
                output_json(&output)?;
            } else {
                print_summary(&results, &run_args.target, output_mode, false);
            }

            // Return error if any failed (for exit code)
            let failed = results.iter().filter(|r| !r.success).count();
            if failed > 0 {
                return Err(anyhow::anyhow!("{failed} project(s) failed"));
            }
        }
    }

    Ok(())
}

/// Recursively collect target dependencies from the target graph
fn collect_target_deps_recursive(
    target: &str,
    graph: &TargetGraph,
    collected: &mut std::collections::HashSet<String>,
) {
    for dep in graph.dependencies(target) {
        if collected.insert(dep.address.clone()) {
            collect_target_deps_recursive(&dep.address, graph, collected);
        }
    }
}

/// Compute DAG levels for heterogeneous target execution
fn compute_heterogeneous_levels(
    targets: &std::collections::HashSet<String>,
    project_map: &std::collections::HashMap<String, &DiscoveredProject>,
) -> Vec<Vec<String>> {
    use std::collections::{HashMap, HashSet};

    // Build dependency map for targets in our set
    let mut deps_map: HashMap<String, HashSet<String>> = HashMap::new();

    for target_addr in targets {
        // Parse target address: //path/to/project:target_name
        let colon_pos = match target_addr.rfind(':') {
            Some(pos) => pos,
            None => continue,
        };
        let project_addr = &target_addr[..colon_pos];
        let target_name = &target_addr[colon_pos + 1..];

        let project = match project_map.get(project_addr) {
            Some(p) => p,
            None => continue,
        };

        let target_deps: HashSet<String> = project
            .targets
            .get(target_name)
            .map(|t| {
                t.depends_on
                    .iter()
                    .filter(|d| targets.contains(*d))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        deps_map.insert(target_addr.clone(), target_deps);
    }

    // Track which level each target is assigned to
    let mut level_of: HashMap<String, usize> = HashMap::new();
    let mut remaining: HashSet<String> = targets.clone();
    let mut current_level = 0;

    while !remaining.is_empty() {
        let mut this_level = Vec::new();

        for addr in &remaining {
            let deps = deps_map.get(addr).cloned().unwrap_or_default();

            let all_deps_assigned = deps
                .iter()
                .all(|d| level_of.get(d).map(|l| *l < current_level).unwrap_or(false));

            let no_deps = deps.is_empty();

            if all_deps_assigned || no_deps {
                this_level.push(addr.clone());
            }
        }

        // If we made no progress, there's a cycle - just add remaining
        if this_level.is_empty() && !remaining.is_empty() {
            this_level = remaining.iter().cloned().collect();
        }

        for addr in &this_level {
            level_of.insert(addr.clone(), current_level);
            remaining.remove(addr);
        }

        current_level += 1;
    }

    // Build level vectors
    let max_level = level_of.values().max().copied().unwrap_or(0);
    let mut levels: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];

    for (addr, level) in level_of {
        levels[level].push(addr);
    }

    // Sort each level for deterministic output
    for level in &mut levels {
        level.sort();
    }

    levels
}

/// Execute heterogeneous targets level by level
fn execute_heterogeneous(
    _executor: &Executor,
    levels: &[Vec<String>],
    project_map: &std::collections::HashMap<String, &DiscoveredProject>,
    output_mode: OutputMode,
    _full_logs: bool,
) -> Vec<aster::executor::ExecutionResult> {
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use aster::ui::ProgressDisplay;

    // Show progress in Normal mode (ProgressDisplay handles terminal vs CI mode internally)
    let show_progress = output_mode == OutputMode::Normal;
    let mut progress = ProgressDisplay::new(show_progress);
    let mut all_results = Vec::new();

    for level in levels {
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();

        for target_addr in level {
            // Parse target address
            let colon_pos = match target_addr.rfind(':') {
                Some(pos) => pos,
                None => continue,
            };
            let project_addr = &target_addr[..colon_pos];
            let target_name = &target_addr[colon_pos + 1..];

            let project = match project_map.get(project_addr) {
                Some(p) => *p,
                None => continue,
            };

            let command = match project.targets.get(target_name) {
                Some(t) => t.command.clone(),
                None => {
                    let result = aster::executor::ExecutionResult {
                        address: target_addr.clone(),
                        success: true,
                        skipped: true,
                        cached: false,
                        output: format!("Skipped: no '{target_name}' target defined"),
                        duration_ms: 0,
                    };
                    if show_progress {
                        progress.add_running(target_addr);
                        progress.mark_complete(target_addr, true, true, 0);
                    }
                    let _ = tx.send(result);
                    continue;
                }
            };

            if show_progress {
                progress.add_running(target_addr);
            }

            let addr = target_addr.clone();
            let project_root = project.root.clone();
            let tx_clone = tx.clone();

            let handle = thread::spawn(move || {
                let start = Instant::now();
                let parts: Vec<&str> = command.split_whitespace().collect();

                let result = if parts.is_empty() {
                    aster::executor::ExecutionResult {
                        address: addr.clone(),
                        success: false,
                        skipped: false,
                        cached: false,
                        output: "Empty command".to_string(),
                        duration_ms: 0,
                    }
                } else {
                    let output = Command::new(parts[0])
                        .args(&parts[1..])
                        .current_dir(&project_root)
                        .output();

                    let duration_ms = start.elapsed().as_millis();

                    match output {
                        Ok(out) => {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            let mut combined = String::new();
                            if !stdout.is_empty() {
                                combined.push_str(&stdout);
                            }
                            if !stderr.is_empty() {
                                if !combined.is_empty() && !combined.ends_with('\n') {
                                    combined.push('\n');
                                }
                                combined.push_str(&stderr);
                            }

                            aster::executor::ExecutionResult {
                                address: addr,
                                success: out.status.success(),
                                skipped: false,
                                cached: false,
                                output: combined,
                                duration_ms,
                            }
                        }
                        Err(e) => aster::executor::ExecutionResult {
                            address: addr,
                            success: false,
                            skipped: false,
                            cached: false,
                            output: format!("Failed to execute: {e}"),
                            duration_ms,
                        },
                    }
                };

                let _ = tx_clone.send(result.clone());
                result
            });

            handles.push(handle);
        }

        drop(tx);

        for result in rx.iter() {
            if show_progress {
                progress.mark_complete(
                    &result.address,
                    result.success,
                    result.skipped,
                    result.duration_ms,
                );
            }
            all_results.push(result);
        }

        for handle in handles {
            let _ = handle.join();
        }
    }

    all_results
}

/// Print summary for heterogeneous execution
fn print_heterogeneous_summary(
    results: &[aster::executor::ExecutionResult],
    output_mode: OutputMode,
    full_logs: bool,
) {
    use console::style;

    let passed = results.iter().filter(|r| r.success && !r.skipped).count();
    let failed = results.iter().filter(|r| !r.success && !r.skipped).count();
    let skipped = results.iter().filter(|r| r.skipped).count();

    if output_mode == OutputMode::Quiet {
        println!("{passed} passed, {failed} failed");
        return;
    }

    // Print failure details
    for result in results.iter().filter(|r| !r.success && !r.skipped) {
        eprintln!();
        eprintln!("{} {}", style("FAILED").red().bold(), result.address);
        let lines: Vec<&str> = result.output.lines().collect();
        let output_lines = if full_logs {
            // Full output mode - show all lines
            &lines[..]
        } else {
            // Truncated mode - show last 15 lines
            if lines.len() > 15 {
                &lines[lines.len() - 15..]
            } else {
                &lines[..]
            }
        };
        for line in output_lines {
            eprintln!("    {line}");
        }
        // Only show hint if not already showing full logs
        if !full_logs {
            eprintln!(
                "    {}",
                style(format!(
                    "Run `aster logs {}` for full output",
                    result.address
                ))
                .dim()
            );
        }
    }

    println!();
    println!(
        "{} {} passed, {} failed{}",
        if failed > 0 {
            style("✗").red()
        } else {
            style("✓").green()
        },
        passed,
        failed,
        if skipped > 0 {
            format!(", {skipped} skipped")
        } else {
            String::new()
        }
    );
}

/// Handle the init command
///
/// Creates aster.toml in either:
/// - The existing workspace root (if in a git repo)
/// - The current directory (if no workspace exists)
fn handle_init(cwd: &std::path::Path, verbose: bool) -> Result<()> {
    // Try to find an existing workspace root (git repo or existing aster.toml)
    let workspace_root = find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());

    // Check if aster.toml already exists
    let aster_toml_path = workspace_root.join("aster.toml");
    if aster_toml_path.exists() {
        return Err(anyhow::anyhow!(
            "aster.toml already exists at {}",
            aster_toml_path.display()
        ));
    }

    if verbose {
        eprintln!("Workspace root: {}", workspace_root.display());
    }

    // Write minimal aster.toml
    let content = r#"# Aster workspace configuration
# This file marks the root of an aster workspace.
# See https://github.com/archastro/aster for documentation.
"#;
    fs::write(&aster_toml_path, content)
        .with_context(|| format!("Failed to write {}", aster_toml_path.display()))?;

    println!("Created {}", aster_toml_path.display());

    // Set up plugin registry
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(NodeJsPlugin));
    registry.register(Box::new(ElixirPlugin));
    registry.register(Box::new(PythonPlugin));
    registry.register(Box::new(GoPlugin));

    // Discover projects
    let projects =
        discover_projects(&workspace_root, &registry).context("Failed to discover projects")?;

    // Print discovery summary
    if projects.is_empty() {
        println!("No projects discovered yet.");
    } else {
        print!("Found {} projects: ", projects.len());
        let addrs: Vec<String> = projects
            .iter()
            .map(|p| format!("//{}", p.relative_path.display()))
            .collect();
        println!("{}", addrs.join(", "));
    }

    Ok(())
}

/// Handle project subcommands
fn handle_project_command(
    command: ProjectCommands,
    workspace_root: &Path,
    projects: &[DiscoveredProject],
    output_mode: OutputMode,
) -> Result<()> {
    match command {
        ProjectCommands::Init { path, force } => {
            handle_project_init(&path, workspace_root, projects, force, output_mode)
        }
    }
}

/// Handle `aster project init` command
fn handle_project_init(
    path: &str,
    workspace_root: &Path,
    projects: &[DiscoveredProject],
    force: bool,
    output_mode: OutputMode,
) -> Result<()> {
    // Resolve the target directory
    let target_dir = if path == "." {
        env::current_dir().context("Failed to get current directory")?
    } else {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            env::current_dir()?.join(p)
        }
    };

    let aster_toml_path = target_dir.join("aster.toml");

    // Check if aster.toml already exists
    if aster_toml_path.exists() && !force {
        return Err(anyhow::anyhow!(
            "aster.toml already exists at {}. Use --force to overwrite.",
            aster_toml_path.display()
        ));
    }

    // Find if there's already a discovered project at this path
    let relative_path = target_dir
        .strip_prefix(workspace_root)
        .unwrap_or(&target_dir);

    let existing_project = projects.iter().find(|p| p.relative_path == relative_path);

    // Detect language plugin by looking for marker files
    let detected_plugin = detect_plugin_for_directory(&target_dir);

    // Generate the aster.toml content
    let content = generate_aster_toml_content(existing_project, detected_plugin.as_deref());

    // Write the file
    fs::write(&aster_toml_path, &content)
        .with_context(|| format!("Failed to write {}", aster_toml_path.display()))?;

    if output_mode != OutputMode::Quiet {
        println!("Created {}", aster_toml_path.display());
        if let Some(plugin) = detected_plugin {
            println!("Detected language: {plugin}");
        } else {
            println!("No language detected - using generic template");
        }
    }

    Ok(())
}

/// Detect which language plugin applies to a directory
fn detect_plugin_for_directory(dir: &Path) -> Option<String> {
    let markers = [
        ("package.json", "nodejs"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("mix.exs", "elixir"),
    ];

    for (marker, plugin_name) in markers {
        if dir.join(marker).exists() {
            return Some(plugin_name.to_string());
        }
    }

    None
}

/// Generate aster.toml content based on detected language
fn generate_aster_toml_content(
    existing_project: Option<&DiscoveredProject>,
    detected_plugin: Option<&str>,
) -> String {
    let mut content = String::new();

    content.push_str("# Project configuration for aster\n");
    content.push_str("# See https://github.com/archastro/aster for documentation\n");
    content.push('\n');

    // Add project name section
    if let Some(project) = existing_project {
        content.push_str(&format!(
            "# Detected project name: \"{}\"\n",
            project.metadata.name
        ));
        content.push_str("# Uncomment to override:\n");
        content.push_str(&format!("# name = \"{}\"\n", project.metadata.name));
    } else {
        content.push_str("# Override the auto-detected project name\n");
        content.push_str("# name = \"my-project\"\n");
    }

    content.push('\n');
    content.push_str("# Add explicit dependencies on other projects\n");
    content.push_str("# depends_on = [\"//libs/shared\", \"//libs/utils:build\"]\n");
    content.push('\n');

    // Add language-specific target examples
    let target_examples = match detected_plugin {
        Some("nodejs") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "npm run test:ci"
# lint = "npm run lint -- --fix"

# Rich format - full control over target behavior:
# [targets.test]
# command = "npm test -- {files}"
# depends_on = ["//self:deps", "//libs/shared:build"]
# capabilities = ["files_list"]
# files_glob = "*.test.ts"

# [targets.typecheck]
# command = "tsc --noEmit"
# depends_on = ["//self:deps"]
"#
        }
        Some("go") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "go test -race ./..."
# build = "go build -o bin/app ./cmd/app"

# Rich format - full control over target behavior:
# [targets.test]
# command = "go test {files}"
# depends_on = ["//self:deps"]
# capabilities = ["files_list"]
# files_glob = "*_test.go"

# [targets.integration]
# command = "go test -tags=integration ./..."
# depends_on = ["//self:build", "//services/db:up"]
"#
        }
        Some("python") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "pytest -v"
# lint = "ruff check --fix ."

# Rich format - full control over target behavior:
# [targets.test]
# command = "pytest {files}"
# depends_on = ["//self:deps"]
# capabilities = ["files_list"]
# files_glob = "*_test.py"

# [targets.typecheck]
# command = "mypy ."
# depends_on = ["//self:deps"]
"#
        }
        Some("elixir") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "mix test --cover"
# lint = "mix credo --strict"

# Rich format - full control over target behavior:
# [targets.test]
# command = "mix test {files}"
# depends_on = ["//self:deps"]
# capabilities = ["files_list"]
# files_glob = "*_test.exs"

# [targets.dialyzer]
# command = "mix dialyzer"
# depends_on = ["//self:build"]
"#
        }
        _ => {
            r#"# Target configuration
# Define custom targets for your project:

# Simple format - just a command:
# [targets]
# test = "make test"
# build = "make build"
# lint = "make lint"

# Rich format - full control over target behavior:
# [targets.test]
# command = "make test ARGS='{files}'"
# depends_on = ["//self:build"]
# capabilities = ["files_list"]
# files_glob = "*_test.*"

# [targets.deploy]
# command = "./scripts/deploy.sh"
# depends_on = ["//self:build", "//self:test"]
"#
        }
    };

    content.push_str(target_examples);
    content
}

/// Apply files to a command, handling both {files} placeholder and plugin-based file injection
///
/// Returns Some(modified_command) if files were applied, None to use original command.
fn apply_files_to_command(
    target: &Target,
    files: &[PathBuf],
    target_name: &str,
    plugin_name: &str,
    registry: &PluginRegistry,
) -> Option<String> {
    if files.is_empty() {
        return None;
    }

    // Filter files by files_glob if specified
    let filtered_files: Vec<PathBuf> = if let Some(glob_pattern) = &target.files_glob {
        match Glob::new(glob_pattern) {
            Ok(glob) => {
                let matcher: GlobMatcher = glob.compile_matcher();
                files
                    .iter()
                    .filter(|f| {
                        // Match against filename only
                        f.file_name()
                            .map(|name| matcher.is_match(name))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            }
            Err(_) => files.to_vec(), // Invalid glob, use all files
        }
    } else {
        files.to_vec()
    };

    if filtered_files.is_empty() {
        return None;
    }

    // Check if command uses {files} placeholder
    if target.command.contains("{files}") {
        let file_list = filtered_files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");

        // Replace {files} with file list (may be empty, which results in trimmed command)
        let modified = target.command.replace("{files}", &file_list);
        // Trim extra whitespace that may result from empty replacement
        let modified = modified.split_whitespace().collect::<Vec<_>>().join(" ");
        return Some(modified);
    }

    // Fall back to plugin's with_files_list for language-specific handling
    if let Some(plugin) = registry.find_by_name(plugin_name) {
        return plugin.with_files_list(target_name, &target.command, &filtered_files);
    }

    None
}

/// Apply warnings-as-errors to a command
///
/// Returns Some(modified_command) if the target supports warnings-as-errors,
/// None otherwise.
fn apply_warnings_as_errors(
    target: &Target,
    target_name: &str,
    plugin_name: &str,
    registry: &PluginRegistry,
) -> Option<String> {
    // Check if target has WarningsAsErrors capability
    if !target
        .capabilities
        .contains(&TargetCapability::WarningsAsErrors)
    {
        return None;
    }

    // Use plugin's with_warnings_as_errors for language-specific handling
    if let Some(plugin) = registry.find_by_name(plugin_name) {
        return plugin.with_warnings_as_errors(target_name, &target.command);
    }

    None
}

/// Format a timestamp as a human-readable relative time
///
/// Parses an RFC3339 timestamp and returns a string like "2 minutes ago"
/// Falls back to the raw timestamp if parsing fails.
fn format_relative_time(timestamp: &str) -> String {
    let parsed: Result<DateTime<Utc>, _> = timestamp.parse();
    match parsed {
        Ok(dt) => {
            let now = Utc::now();
            let duration = now.signed_duration_since(dt);

            if duration.num_seconds() < 0 {
                // Future time (shouldn't happen, but handle gracefully)
                return timestamp.to_string();
            }

            let seconds = duration.num_seconds();
            if seconds < 60 {
                return "just now".to_string();
            }

            let minutes = duration.num_minutes();
            if minutes < 60 {
                return if minutes == 1 {
                    "1 minute ago".to_string()
                } else {
                    format!("{minutes} minutes ago")
                };
            }

            let hours = duration.num_hours();
            if hours < 24 {
                return if hours == 1 {
                    "1 hour ago".to_string()
                } else {
                    format!("{hours} hours ago")
                };
            }

            let days = duration.num_days();
            if days < 30 {
                return if days == 1 {
                    "1 day ago".to_string()
                } else {
                    format!("{days} days ago")
                };
            }

            // Fall back to date for older entries
            dt.format("%Y-%m-%d").to_string()
        }
        Err(_) => timestamp.to_string(),
    }
}
