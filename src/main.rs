//! Aster CLI entry point
//!
//! Build orchestration for polyglot monorepos.

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::fs;
use std::process::ExitCode;

use aster::cli::{
    build_execution_output, check_reserved_target, expand_selection, output_json, parse_run_args,
    print_summary, select_projects, Cli, Commands, GraphOutput, OutputMode, ProjectInfo, WhyOutput,
};
use aster::executor::logs::LogStore;
use aster::config::find_workspace_root;
use aster::discovery::{discover_projects, DiscoveredProject};
use aster::executor::Executor;
use aster::git::{affected_with_dependents, files_to_projects, AffectedDetector};
use aster::graph::{build_graph, build_target_graph, find_cycle, format_path, TargetGraph};
use aster::plugins::{ElixirPlugin, NodeJsPlugin, PluginRegistry, PythonPlugin, TargetCapability};
use std::collections::HashMap;
use std::path::PathBuf;

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

    let cwd = env::current_dir().context("Failed to get current directory")?;

    // Handle init command specially - it works even without an existing workspace
    if matches!(cli.command, Commands::Init) {
        return handle_init(&cwd, cli.verbose);
    }

    // For all other commands, require a workspace
    let workspace_root = find_workspace_root(&cwd)
        .context("Not in an aster workspace (no aster.toml or .git found). Run 'aster init' to create one.")?;

    if output_mode == OutputMode::Verbose {
        eprintln!("[aster] Workspace root: {}", workspace_root.display());
    }

    // Set up plugin registry with all language plugins
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(NodeJsPlugin));
    registry.register(Box::new(ElixirPlugin));
    registry.register(Box::new(PythonPlugin));

    // Discover projects
    let projects = discover_projects(&workspace_root, &registry)
        .context("Failed to discover projects")?;

    if output_mode == OutputMode::Verbose {
        eprintln!("[aster] Discovered {} projects", projects.len());
    }

    match cli.command {
        Commands::Init => unreachable!("Init handled above"),
        Commands::List => {
            if output_mode == OutputMode::Json {
                // JSON output: array of ProjectInfo
                let project_infos: Vec<ProjectInfo> = projects
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
                // Normal or Verbose: text output
                for project in &projects {
                    println!("//{}", project.relative_path.display());

                    if !project.targets.is_empty() {
                        // Sort targets for consistent output
                        let mut target_names: Vec<&str> =
                            project.targets.keys().map(|s| s.as_str()).collect();
                        target_names.sort();

                        for name in target_names {
                            let target = &project.targets[name];
                            if target.depends_on.is_empty() {
                                println!("  {}: {}", name, target.command);
                            } else {
                                println!(
                                    "  {}: {} -> [{}]",
                                    name,
                                    target.command,
                                    target.depends_on.join(", ")
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
                        let dep_addrs: Vec<String> = deps.iter().map(|d| d.address.clone()).collect();
                        edges.insert(node.address.clone(), dep_addrs);
                    }

                    (nodes, edges)
                };

                let output = GraphOutput { nodes, edges };
                output_json(&output)?;
            } else if output_mode == OutputMode::Quiet {
                // Quiet mode: no output for graph
            } else {
                // Normal/Verbose: text output
                if let Some(addr) = target {
                    // Show deps for specific target
                    if let Some(node) = graph.get(&addr) {
                        println!("{}", node.address);
                        for dep in graph.dependencies(&addr) {
                            println!("  -> {}", dep.address);
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
                            println!("{}", current_project);
                        }
                        print!("  :{}", node.target_name);
                        let deps = graph.dependencies(&node.address);
                        if deps.is_empty() {
                            println!();
                        } else {
                            let dep_strs: Vec<&str> = deps.iter().map(|d| d.address.as_str()).collect();
                            println!(" -> [{}]", dep_strs.join(", "));
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
                return Err(anyhow::anyhow!("Target not found: {}", from));
            }
            if graph.get(&to).is_none() {
                return Err(anyhow::anyhow!("Target not found: {}", to));
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
                        println!("No dependency path found between {} and {}", from, to);
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
                eprintln!("[aster] Directly affected: {:?}", directly_affected);
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
                if output_mode != OutputMode::Json {
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
            }

            // Dry run: just show what would run
            if dry_run {
                if output_mode == OutputMode::Json {
                    // Output JSON list of affected projects
                    #[derive(serde::Serialize)]
                    struct DryRunOutput {
                        target: String,
                        base: String,
                        head: Option<String>,
                        affected: Vec<String>,
                        count: usize,
                    }
                    let affected: Vec<String> = ordered
                        .iter()
                        .map(|p| format!("//{}", p.relative_path.display()))
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
                    println!("Would run '{}' on {} projects", target, ordered.len());
                }
                return Ok(());
            }

            // Execute target on projects
            let executor = Executor::with_output_mode(&workspace_root, output_mode);
            let results = if only_affected_files {
                // Build command overrides for targets that support files-list
                let mut command_overrides: HashMap<String, String> = HashMap::new();

                // Create plugin registry to call with_files_list
                let mut registry = PluginRegistry::new();
                registry.register(Box::new(NodeJsPlugin));
                registry.register(Box::new(PythonPlugin));
                registry.register(Box::new(ElixirPlugin));

                for project in &ordered {
                    let project_addr = format!("//{}", project.relative_path.display());
                    let target_addr = format!("{}:{}", project_addr, target);

                    // Check if target has FilesList capability
                    if let Some(target_def) = project.targets.get(&target) {
                        if target_def.capabilities.contains(&TargetCapability::FilesList) {
                            // Get files that belong to this project
                            let project_files: Vec<PathBuf> = changed_files
                                .iter()
                                .filter(|f| f.starts_with(&project.relative_path))
                                .map(|f| {
                                    // Make path relative to project directory
                                    f.strip_prefix(&project.relative_path)
                                        .unwrap_or(f)
                                        .to_path_buf()
                                })
                                .collect();

                            // Call plugin's with_files_list to get modified command
                            if !project_files.is_empty() {
                                if let Some(plugin) = registry.find_by_name(&project.plugin_name) {
                                    if let Some(modified_cmd) = plugin.with_files_list(
                                        &target,
                                        &target_def.command,
                                        &project_files,
                                    ) {
                                        command_overrides.insert(target_addr, modified_cmd);
                                    }
                                }
                            }
                        }
                    }
                }

                if output_mode == OutputMode::Verbose && !command_overrides.is_empty() {
                    eprintln!("[aster] Using modified commands for {} targets", command_overrides.len());
                }

                executor.execute_with_command_overrides(&target, &ordered, &command_overrides)
            } else {
                executor.execute(&target, &ordered, &graph)
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
                return Err(anyhow::anyhow!("{} project(s) failed", failed));
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
                    invalid_targets.push(format!("{} (missing :target suffix)", target));
                } else if target_graph.get(target).is_none() {
                    invalid_targets.push(format!("{} (not found)", target));
                }
            }

            if !invalid_targets.is_empty() {
                return Err(anyhow::anyhow!(
                    "Invalid targets:\n  {}\n\nFormat: //project:target (e.g., //services/api:test)",
                    invalid_targets.join("\n  ")
                ));
            }

            // Collect all targets to run (and optionally their dependencies)
            let mut targets_to_run: std::collections::HashSet<String> = targets.iter().cloned().collect();

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
                eprintln!("[aster] Running {} targets across {} levels", targets_to_run.len(), levels.len());
                for (i, level) in levels.iter().enumerate() {
                    eprintln!("[aster] Level {}: {:?}", i, level);
                }
            }

            // Execute using the executor's infrastructure
            let executor = Executor::with_output_mode(&workspace_root, output_mode);
            let results = execute_heterogeneous(&executor, &levels, &project_map, output_mode);

            // Output results based on mode
            if output_mode == OutputMode::Json {
                let output = build_execution_output(&results);
                output_json(&output)?;
            } else {
                print_heterogeneous_summary(&results, output_mode);
            }

            // Return error if any failed (for exit code)
            let failed = results.iter().filter(|r| !r.success && !r.skipped).count();
            if failed > 0 {
                return Err(anyhow::anyhow!("{} target(s) failed", failed));
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
                return Err(anyhow::anyhow!("{}", err_msg));
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
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            // Expand selection based on flags
            let selected = expand_selection(&run_args, &initial, &graph, &projects);

            // Sort by dependency order
            let ordered = graph.topological_order_subset(&selected);

            if output_mode == OutputMode::Verbose {
                eprintln!(
                    "[aster] Running '{}' on {} projects",
                    run_args.target,
                    ordered.len()
                );
            }

            // Execute target on projects
            let executor = Executor::with_output_mode(&workspace_root, output_mode);
            let results = executor.execute(&run_args.target, &ordered, &graph);

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
                return Err(anyhow::anyhow!("{} project(s) failed", failed));
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
) -> Vec<aster::executor::ExecutionResult> {
    use std::io::IsTerminal;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use aster::ui::ProgressDisplay;

    let show_progress = output_mode == OutputMode::Normal && std::io::stderr().is_terminal();
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
                        output: format!("Skipped: no '{}' target defined", target_name),
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
                                output: combined,
                                duration_ms,
                            }
                        }
                        Err(e) => aster::executor::ExecutionResult {
                            address: addr,
                            success: false,
                            skipped: false,
                            output: format!("Failed to execute: {}", e),
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
                progress.mark_complete(&result.address, result.success, result.skipped, result.duration_ms);
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
) {
    use console::style;

    let passed = results.iter().filter(|r| r.success && !r.skipped).count();
    let failed = results.iter().filter(|r| !r.success && !r.skipped).count();
    let skipped = results.iter().filter(|r| r.skipped).count();

    if output_mode == OutputMode::Quiet {
        println!("{} passed, {} failed", passed, failed);
        return;
    }

    // Print failure details
    for result in results.iter().filter(|r| !r.success && !r.skipped) {
        eprintln!();
        eprintln!("{} {}", style("FAILED").red().bold(), result.address);
        let lines: Vec<&str> = result.output.lines().collect();
        let tail = if lines.len() > 15 { &lines[lines.len() - 15..] } else { &lines[..] };
        for line in tail {
            eprintln!("    {}", line);
        }
        eprintln!("    {}", style(format!("Run `aster logs {}` for full output", result.address)).dim());
    }

    println!();
    println!(
        "{} {} passed, {} failed{}",
        if failed > 0 { style("✗").red() } else { style("✓").green() },
        passed,
        failed,
        if skipped > 0 { format!(", {} skipped", skipped) } else { String::new() }
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

    // Discover projects
    let projects = discover_projects(&workspace_root, &registry)
        .context("Failed to discover projects")?;

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
