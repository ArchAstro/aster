//! Aster CLI entry point
//!
//! Build orchestration for polyglot monorepos.

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;

use aster::cli::{
    build_execution_output, check_reserved_target, expand_selection, output_json, parse_run_args,
    print_summary, select_projects, Cli, Commands, GraphOutput, OutputMode, ProjectCommands,
    ProjectInfo, ServicesCommands, TlsCommands, WhyOutput, SKILLS_MARKDOWN,
};
use aster::config::{find_workspace_root, WorkspaceConfig};
use aster::discovery::{discover_projects, DiscoveredProject};
use aster::executor::logs::LogStore;
use aster::executor::{
    collect_target_deps, compute_target_levels, install_signal_handler, parse_target_address,
    shutdown_signal, Executor,
};
use aster::git::{affected_with_dependents, files_to_projects, AffectedDetector, AffectedIgnore};
use aster::graph::{build_graph, build_target_graph, find_cycle, format_path};
use aster::plugins::{PluginRegistry, Target, TargetCapability};
use chrono::{DateTime, Utc};
use globset::{Glob, GlobMatcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Number of characters to show when displaying truncated cache hashes
const CACHE_HASH_DISPLAY_LENGTH: usize = 8;

fn main() -> ExitCode {
    install_signal_handler();
    let result = run();
    if let Some(signal) = shutdown_signal() {
        return ExitCode::from(u8::try_from(128 + signal).unwrap_or(u8::MAX));
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut cli = Cli::parse();
    if cli.skills {
        return print_skills();
    }
    let command = cli
        .command
        .take()
        .expect("clap requires either --skills or a subcommand");
    let output_mode = cli.output_mode();
    let full_logs = cli.full_logs();

    let cwd = env::current_dir().context("Failed to get current directory")?;

    // Handle init command specially - it works even without an existing workspace
    if matches!(command, Commands::Init) {
        return handle_init(&cwd, cli.verbose);
    }

    // Explicit numeric port cleanup is useful even outside an Aster workspace.
    // Named/default selection still loads the workspace configuration below.
    if let Commands::Services {
        command: ServicesCommands::KillPorts { ports, dry_run },
    } = &command
    {
        if !ports.is_empty() && ports.iter().all(|port| port.parse::<u16>().is_ok()) {
            let selected = aster::dev::resolve_port_selection(&HashMap::new(), ports)?;
            return aster::dev::kill_ports(
                &selected,
                aster::dev::KillPortsOptions { dry_run: *dry_run },
            );
        }
    }

    // For all other commands, require a workspace
    let workspace_root = find_workspace_root(&cwd).context(
        "Not in an aster workspace (no aster.toml or .git found). Run 'aster init' to create one.",
    )?;

    // Reading existing logs does not require project discovery or graph validation.
    if let Commands::Services {
        command: ServicesCommands::Logs { service },
    } = &command
    {
        let workspace_config = WorkspaceConfig::load(&workspace_root)?;
        return aster::dev::show_service_logs(&workspace_root, &workspace_config.dev, service);
    }

    // TLS setup and serving only need workspace service configuration. Handling
    // them before discovery lets a supervised TLS target remain independent of
    // the repository's project graph.
    if let Commands::Services {
        command: ServicesCommands::Tls { command },
    } = &command
    {
        let workspace_config = WorkspaceConfig::load(&workspace_root)?;
        return match command {
            TlsCommands::Setup { edge } => {
                aster::dev::setup_tls(&workspace_root, &workspace_config.dev, edge)
            }
            TlsCommands::Serve { edge } => {
                aster::dev::serve_tls(&workspace_root, &workspace_config.dev, edge)
            }
        };
    }

    if output_mode == OutputMode::Verbose {
        eprintln!("[aster] Workspace root: {}", workspace_root.display());
    }

    // Set up plugin registry with all language plugins
    let registry = PluginRegistry::with_all_plugins();

    // Discover projects
    let projects =
        discover_projects(&workspace_root, &registry).context("Failed to discover projects")?;

    if output_mode == OutputMode::Verbose {
        eprintln!("[aster] Discovered {} projects", projects.len());
    }

    let explicit_target = matches!(&command, Commands::RunTarget { .. });
    match command {
        Commands::Init => unreachable!("Init handled above"),
        Commands::List { path, lang } => {
            validate_lang_filter(&lang)?;

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

            // Apply language filter
            let filtered_projects: Vec<&DiscoveredProject> = if !lang.is_empty() {
                filtered_projects
                    .into_iter()
                    .filter(|p| p.has_any_language(&lang))
                    .collect()
            } else {
                filtered_projects
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
                            languages: p.languages.clone(),
                            build_system: p.build_system.clone(),
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
            lang,
        } => {
            validate_lang_filter(&lang)?;

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

            // Affected ignores are distinct from discovery ignores: they remove
            // matching Git changes before ownership, rationale, and files-list handling.
            let workspace_config = WorkspaceConfig::load(&workspace_root)?;
            let unfiltered_count = changed_files.len();
            let changed_files =
                AffectedIgnore::build(&workspace_config.affected)?.filter(changed_files);

            if output_mode == OutputMode::Verbose {
                eprintln!("[aster] Found {} changed files", changed_files.len());
                let ignored_count = unfiltered_count - changed_files.len();
                if ignored_count > 0 {
                    eprintln!("[aster] Ignored {ignored_count} changed files via affected.ignore");
                }
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

            // Keep a copy of directly affected for rationale tracking in dry-run
            let directly_affected_addrs = directly_affected.clone();

            // Expand with dependents if requested
            let affected_addrs = if dependents {
                affected_with_dependents(directly_affected, &graph)
            } else {
                directly_affected
            };

            // Primary projects are the affected ones (only these run the requested target)
            let primary_addrs: HashSet<String> = affected_addrs.iter().cloned().collect();

            // Find DiscoveredProject refs for affected addresses
            let affected_projects: Vec<_> = projects
                .iter()
                .filter(|p| {
                    let addr = format!("//{}", p.relative_path.display());
                    affected_addrs.contains(&addr)
                })
                .filter(|p| lang.is_empty() || p.has_any_language(&lang))
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

            // Dry run: show the full execution graph with rationale
            if dry_run {
                // Build project map from ALL projects (same as executor would)
                let all_project_map: HashMap<String, &DiscoveredProject> = projects
                    .iter()
                    .map(|p| (format!("//{}", p.relative_path.display()), p))
                    .collect();

                // Compute targets_to_run (same logic as executor)
                let mut targets_to_run: HashSet<String> = HashSet::new();
                for addr in &primary_addrs {
                    let target_addr = format!("{addr}:{target}");
                    targets_to_run.insert(target_addr.clone());
                    collect_target_deps(&target_addr, &all_project_map, &mut targets_to_run);
                }

                // Compute DAG levels for execution order
                let levels = compute_target_levels(&targets_to_run, &all_project_map);

                // Build map of project -> affected files
                let mut files_per_project: HashMap<String, Vec<String>> = HashMap::new();
                for project in &projects {
                    let project_addr = format!("//{}", project.relative_path.display());
                    let project_files: Vec<String> = changed_files
                        .iter()
                        .filter(|f| f.starts_with(&project.relative_path))
                        .map(|f| f.to_string_lossy().to_string())
                        .collect();
                    if !project_files.is_empty() {
                        files_per_project.insert(project_addr, project_files);
                    }
                }

                // Categorize each target's rationale
                let rationale_for = |target_addr: &str| -> &'static str {
                    if let Some((project_addr, target_name)) = parse_target_address(target_addr) {
                        if target_name == target {
                            // This is the requested target
                            if directly_affected_addrs.contains(&project_addr) {
                                return "affected";
                            } else if primary_addrs.contains(&project_addr) {
                                // In primary but not directly affected = added by --dependents
                                return "dependent";
                            }
                        }
                    }
                    "target dependency"
                };

                if output_mode == OutputMode::Json {
                    #[derive(serde::Serialize)]
                    struct DryRunTarget {
                        address: String,
                        reason: String,
                        files: Vec<String>,
                    }
                    #[derive(serde::Serialize)]
                    struct DryRunOutput {
                        target: String,
                        base: String,
                        head: Option<String>,
                        targets: Vec<DryRunTarget>,
                        count: usize,
                    }
                    let mut all_targets = Vec::new();
                    for level in &levels {
                        for target_addr in level {
                            let reason = rationale_for(target_addr).to_string();
                            let project_addr = parse_target_address(target_addr)
                                .map(|(p, _)| p)
                                .unwrap_or_default();
                            let files = if reason == "affected" {
                                files_per_project
                                    .get(&project_addr)
                                    .cloned()
                                    .unwrap_or_default()
                            } else {
                                vec![]
                            };
                            all_targets.push(DryRunTarget {
                                address: target_addr.clone(),
                                reason,
                                files,
                            });
                        }
                    }
                    let count = all_targets.len();
                    let output = DryRunOutput {
                        target: target.clone(),
                        base: base.clone(),
                        head: head.clone(),
                        targets: all_targets,
                        count,
                    };
                    output_json(&output)?;
                } else if output_mode != OutputMode::Quiet {
                    // Count targets by category
                    let dep_count = targets_to_run
                        .iter()
                        .filter(|t| rationale_for(t) == "target dependency")
                        .count();

                    println!();
                    if dep_count > 0 {
                        println!(
                            "Would run '{}' on {} projects (+{} dependency targets):",
                            target,
                            primary_addrs.len(),
                            dep_count
                        );
                    } else {
                        println!(
                            "Would run '{}' on {} projects:",
                            target,
                            primary_addrs.len()
                        );
                    }
                    println!();
                    for level in &levels {
                        for target_addr in level {
                            let reason = rationale_for(target_addr);
                            println!("  {target_addr}  ({reason})");
                            if reason == "affected" {
                                if let Some((project_addr, _)) = parse_target_address(target_addr) {
                                    if let Some(files) = files_per_project.get(&project_addr) {
                                        for file in files {
                                            println!("    - {file}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                return Ok(());
            }

            // Execute target on projects
            // Pass ALL projects so executor can resolve target-level dependencies
            // (e.g., //app:build depends on //lib:build). Only primary (affected)
            // projects will run the requested target.
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache);
            let all_project_refs: Vec<_> = projects.iter().collect();

            // Build command overrides for files-list and warnings-as-errors
            let results = if only_affected_files || warnings_as_errors {
                let mut command_overrides: HashMap<String, String> = HashMap::new();
                let mut effective_primary_addrs = primary_addrs.clone();

                // Create plugin registry for capability handling
                let registry = PluginRegistry::with_all_plugins();

                for project in &ordered {
                    let project_addr = format!("//{}", project.relative_path.display());
                    let target_addr = format!("{project_addr}:{target}");

                    if let Some(target_def) = project.targets.get(&target) {
                        let mut modified_cmd: Option<String> = None;
                        let mut files_list_attempted = false;

                        // Apply files-list if requested and supported
                        if only_affected_files
                            && target_def
                                .capabilities
                                .contains(&TargetCapability::FilesList)
                        {
                            files_list_attempted = true;

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
                            )?;
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
                        } else if files_list_attempted {
                            // --only-affected-files was set and the target supports FilesList,
                            // but no relevant files matched (e.g., only source files changed,
                            // no test files). Skip this project instead of running the full suite.
                            effective_primary_addrs.remove(&project_addr);
                            if output_mode == OutputMode::Verbose {
                                eprintln!(
                                    "[aster] Skipping {target_addr}: no matching files for --only-affected-files"
                                );
                            }
                        }
                    }
                }

                if output_mode == OutputMode::Verbose && !command_overrides.is_empty() {
                    eprintln!(
                        "[aster] Using modified commands for {} targets",
                        command_overrides.len()
                    );
                }

                executor.execute_with_command_overrides(
                    &target,
                    &all_project_refs,
                    &command_overrides,
                    Some(&effective_primary_addrs),
                )
            } else {
                executor.execute(&target, &all_project_refs, &graph, Some(&primary_addrs))
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
        Commands::Run {
            targets,
            no_deps,
            lang,
        } => {
            validate_lang_filter(&lang)?;
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

            // Build project map for lang filtering and executor
            let project_map: std::collections::HashMap<String, &DiscoveredProject> = projects
                .iter()
                .map(|p| (format!("//{}", p.relative_path.display()), p))
                .collect();

            // Apply language filter to requested targets
            let filtered_targets: Vec<String> = if !lang.is_empty() {
                targets
                    .into_iter()
                    .filter(|t| {
                        if let Some((proj_addr, _)) = t.rsplit_once(':') {
                            project_map
                                .get(proj_addr)
                                .map(|p| p.has_any_language(&lang))
                                .unwrap_or(true)
                        } else {
                            true
                        }
                    })
                    .collect()
            } else {
                targets
            };

            if filtered_targets.is_empty() {
                if output_mode != OutputMode::Quiet {
                    println!("No targets match the specified language filter");
                }
                return Ok(());
            }

            let primary_targets: std::collections::HashSet<String> =
                filtered_targets.into_iter().collect();
            let all_project_refs: Vec<_> = projects.iter().collect();
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache);
            let results = executor.execute_targets(&primary_targets, &all_project_refs, no_deps);

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
        Commands::Watch {
            targets,
            target,
            debounce,
            no_initial,
            lang,
        } => {
            validate_lang_filter(&lang)?;

            handle_watch(
                &workspace_root,
                projects,
                targets,
                target,
                debounce,
                no_initial,
                lang,
                output_mode,
                full_logs,
                cli.no_cache,
            )?;
        }
        Commands::Services { command } => match command {
            ServicesCommands::Up {
                group,
                no_watch,
                no_ui,
                dry_run,
            } => {
                let workspace_config = WorkspaceConfig::load(&workspace_root)?;
                let graph = build_target_graph(&projects);
                if let Some(cycle) = graph.find_cycle() {
                    return Err(anyhow::anyhow!("{cycle}"));
                }
                let plan = aster::dev::resolve_dev_plan(
                    &workspace_root,
                    &workspace_config.dev,
                    group.as_deref(),
                    &projects,
                    &graph,
                    &registry,
                )?;
                aster::dev::run_dev(
                    &workspace_root,
                    projects,
                    graph,
                    plan,
                    &workspace_config,
                    aster::dev::DevOptions {
                        watch: !no_watch,
                        ui: !no_ui,
                        dry_run,
                        use_cache: !cli.no_cache,
                    },
                )?;
            }
            ServicesCommands::KillPorts { ports, dry_run } => {
                let workspace_config = WorkspaceConfig::load(&workspace_root)?;
                let configured =
                    aster::dev::resolve_dev_ports(&workspace_root, &workspace_config.dev)?;
                let selected = aster::dev::resolve_port_selection(&configured, &ports)?;
                aster::dev::kill_ports(&selected, aster::dev::KillPortsOptions { dry_run })?;
            }
            ServicesCommands::Logs { .. } => unreachable!("service logs handled before discovery"),
            ServicesCommands::Tls { .. } => unreachable!("TLS commands handled before discovery"),
        },
        Commands::RunTarget { ref args } | Commands::ExternalTarget(ref args) => {
            // Parse external subcommand args
            let run_args = parse_run_args(args.clone());

            if run_args.target.is_empty() {
                return Err(anyhow::anyhow!("No target specified. Usage: aster <target> [projects...] [--all] [--no-deps] [--dependents]"));
            }

            validate_lang_filter(&run_args.lang)?;

            // Apply global flags that clap couldn't parse from external subcommands
            if run_args.no_cache {
                cli.no_cache = true;
            }
            if run_args.verbose {
                cli.verbose = true;
            }
            if run_args.quiet {
                cli.quiet = true;
            }
            if run_args.json {
                cli.json = true;
            }
            if run_args.full_logs {
                cli.full_logs = true;
            }
            let output_mode = cli.output_mode();
            let full_logs = cli.full_logs();

            // Check for reserved command conflicts
            if !explicit_target {
                if let Some(err_msg) = check_reserved_target(&run_args.target) {
                    return Err(anyhow::anyhow!("{err_msg}"));
                }
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
                let registry = PluginRegistry::with_all_plugins();

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

                // Pass ALL projects so executor can resolve target-level dependencies
                // (e.g., workspace member :build depends on workspace root :deps).
                // Only primary projects will run the requested target.
                // With --no-deps, only pass the ordered subset to skip dependency projects entirely.
                let executor_projects: Vec<_> = if run_args.no_deps {
                    ordered.to_vec()
                } else {
                    projects.iter().collect()
                };
                executor.execute_with_command_overrides(
                    &run_args.target,
                    &executor_projects,
                    &command_overrides,
                    Some(&primary_projects),
                )
            } else {
                // Pass ALL projects so executor can resolve target-level dependencies
                // (e.g., workspace member :build depends on workspace root :deps).
                // Only primary projects will run the requested target.
                // With --no-deps, only pass the ordered subset to skip dependency projects entirely.
                let executor_projects: Vec<_> = if run_args.no_deps {
                    ordered.to_vec()
                } else {
                    projects.iter().collect()
                };
                executor.execute(
                    &run_args.target,
                    &executor_projects,
                    &graph,
                    Some(&primary_projects),
                )
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

fn print_skills() -> Result<()> {
    match io::stdout().lock().write_all(SKILLS_MARKDOWN.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).context("Failed to write the Aster skills guide"),
    }
}

/// Print summary for heterogeneous execution
fn print_heterogeneous_summary(
    results: &[aster::executor::ExecutionResult],
    output_mode: OutputMode,
) {
    use console::style;

    let cached = results.iter().filter(|r| r.cached).count();
    let passed = results
        .iter()
        .filter(|r| r.success && !r.skipped && !r.cached)
        .count();
    let failed = results.iter().filter(|r| !r.success && !r.skipped).count();
    let skipped = results.iter().filter(|r| r.skipped).count();
    let cached_summary = if cached > 0 {
        format!(", {cached} cached")
    } else {
        String::new()
    };

    if output_mode == OutputMode::Quiet {
        println!("{passed} passed{cached_summary}, {failed} failed");
        return;
    }

    println!();
    println!(
        "{} {} passed{}, {} failed{}",
        if failed > 0 {
            style("✗").red()
        } else {
            style("✓").green()
        },
        passed,
        cached_summary,
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
    let registry = PluginRegistry::with_all_plugins();

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

/// Handle the `watch` command.
#[allow(clippy::too_many_arguments)]
fn handle_watch(
    workspace_root: &Path,
    projects: Vec<DiscoveredProject>,
    targets: Vec<String>,
    default_target: String,
    debounce: Option<String>,
    no_initial: bool,
    lang: Vec<String>,
    output_mode: OutputMode,
    full_logs: bool,
    no_cache: bool,
) -> Result<()> {
    use aster::config::WorkspaceConfig;
    use aster::watch::{run_watch, WatchOpts, WatchPlan, WorkspaceIgnore};
    use std::time::Duration;

    // Resolve each entry to //project:target. Bare //project uses default_target.
    let mut resolved: Vec<String> = Vec::new();
    for entry in &targets {
        if entry.ends_with("/...") || entry == "//..." {
            return Err(anyhow::anyhow!(
                "glob selectors are not yet supported by `aster watch`; specify targets explicitly: {entry}"
            ));
        }
        if entry.contains(':') {
            resolved.push(entry.clone());
        } else if let Some(rest) = entry.strip_prefix("//") {
            if rest.is_empty() {
                return Err(anyhow::anyhow!("invalid target: {entry}"));
            }
            resolved.push(format!("{entry}:{default_target}"));
        } else {
            return Err(anyhow::anyhow!("target must start with '//': {entry}"));
        }
    }

    // Apply language filter by dropping targets from non-matching projects.
    if !lang.is_empty() {
        let project_languages: HashMap<String, Vec<String>> = projects
            .iter()
            .map(|p| {
                (
                    format!("//{}", p.relative_path.display()),
                    p.languages.clone(),
                )
            })
            .collect();
        resolved.retain(|addr| {
            let project_addr = addr.rsplit_once(':').map(|(p, _)| p).unwrap_or(addr);
            project_languages
                .get(project_addr)
                .map(|project_languages| {
                    lang.iter()
                        .any(|language| project_languages.contains(language))
                })
                .unwrap_or(false)
        });
        if resolved.is_empty() {
            return Err(anyhow::anyhow!(
                "no targets match the specified language filter"
            ));
        }
    }

    // Build target graph for validation + dependents traversal.
    let graph = build_target_graph(&projects);
    if let Some(cycle) = graph.find_cycle() {
        return Err(anyhow::anyhow!("{cycle}"));
    }

    let registry = PluginRegistry::with_all_plugins();

    let plan = WatchPlan::build(&resolved, &projects, &graph, &registry)?;

    let workspace_config = WorkspaceConfig::load(workspace_root)?;
    let ignore = WorkspaceIgnore::build(&workspace_config.watch)?;

    let debounce_duration = match debounce.as_deref() {
        Some(s) => humantime::parse_duration(s)
            .map_err(|e| anyhow::anyhow!("invalid --debounce '{s}': {e}"))?,
        None => Duration::from_millis(workspace_config.watch.debounce_ms.unwrap_or(300)),
    };

    let opts = WatchOpts {
        debounce: debounce_duration,
        run_initial: !no_initial,
        ..Default::default()
    };

    let executor = Executor::with_all_options(workspace_root, output_mode, full_logs, !no_cache);

    run_watch(
        plan,
        workspace_root.to_path_buf(),
        projects,
        graph,
        executor,
        ignore,
        opts,
    )
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
        ("Gemfile", "ruby"),
        ("package.json", "nodejs"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("mix.exs", "elixir"),
        ("Cargo.toml", "rust"),
        ("pom.xml", "maven"),
        ("settings.gradle.kts", "gradle"),
        ("settings.gradle", "gradle"),
        ("build.gradle.kts", "gradle"),
        ("build.gradle", "gradle"),
    ];

    for (marker, plugin_name) in markers {
        if dir.join(marker).exists() {
            return Some(plugin_name.to_string());
        }
    }

    if std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "gemspec")
        })
    {
        return Some("ruby".to_string());
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
        Some("gradle") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "./gradlew test"
# lint = "./gradlew check"

# Rich format - full control over target behavior:
# [targets.integration]
# command = "./gradlew integrationTest"
# depends_on = ["//self:build"]
"#
        }
        Some("maven") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "./mvnw test"
# lint = "./mvnw verify -DskipTests"

# Rich format - full control over target behavior:
# [targets.integration]
# command = "./mvnw verify -Pintegration"
# depends_on = ["//self:build"]
"#
        }
        Some("ruby") => {
            r#"# Target configuration
# Simple format - just override the command:
# [targets]
# test = "bundle exec rake test"
# lint = "bundle exec rubocop"

# Rich format - full control over target behavior:
# [targets.integration]
# command = "bundle exec rspec spec/integration"
# depends_on = ["//self:deps"]
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
# command = "make test {files}"
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
) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
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
        return Ok(None);
    }

    // Expand only standalone argv placeholders. Textual substitution inside
    // an already quoted argument (especially `sh -c '... {files}'`) can turn a
    // filename into shell syntax after the outer command is parsed.
    if target.command.contains("{files}") {
        let parts = shell_words::split(&target.command)
            .with_context(|| format!("invalid command quoting: {}", target.command))?;
        if parts
            .iter()
            .any(|part| part.contains("{files}") && part != "{files}")
        {
            anyhow::bail!(
                "{{files}} must be a standalone command argument, not embedded in a quoted or \
                 combined argument: {}",
                target.command
            );
        }
        let program_index = parts
            .iter()
            .position(|part| !is_environment_assignment(part))
            .context("command contains only environment assignments")?;
        let placeholder_positions = parts
            .iter()
            .enumerate()
            .filter_map(|(index, part)| (part == "{files}").then_some(index))
            .collect::<Vec<_>>();
        if placeholder_positions.len() != 1 {
            anyhow::bail!("command must contain exactly one {{files}} placeholder");
        }
        let placeholder_index = placeholder_positions[0];
        if placeholder_index == program_index {
            anyhow::bail!("{{files}} cannot be used as the executable");
        }
        if parts[program_index..placeholder_index]
            .iter()
            .any(|part| is_command_interpreter_token(part))
        {
            anyhow::bail!(
                "{{files}} cannot be expanded directly through a command interpreter; \
                 use a fixed wrapper executable instead"
            );
        }

        let mut expanded = Vec::new();
        for part in parts {
            if part == "{files}" {
                expanded.extend(
                    filtered_files
                        .iter()
                        .map(|file| file.to_string_lossy().into_owned()),
                );
            } else {
                expanded.push(part);
            }
        }
        return Ok(Some(
            expanded
                .iter()
                .map(|part| aster::executor::quote_command_argument(part))
                .collect::<Vec<_>>()
                .join(" "),
        ));
    }

    // Fall back to plugin's with_files_list for language-specific handling
    if let Some(plugin) = registry.find_by_name(plugin_name) {
        return Ok(plugin.with_files_list(target_name, &target.command, &filtered_files));
    }

    Ok(None)
}

fn is_environment_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_command_interpreter_token(value: &str) -> bool {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "ksh"
            | "fish"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "env"
            | "sudo"
            | "xargs"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "ruby"
            | "perl"
            | "php"
            | "lua"
    ) || name.starts_with("python")
        || name.starts_with("pypy")
        || name.starts_with("ruby")
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
/// Known source-language names for --lang validation
const VALID_LANGS: &[&str] = &[
    "nodejs", "python", "rust", "go", "elixir", "java", "kotlin", "ruby",
];

/// Validate that all --lang values are known source languages
fn validate_lang_filter(langs: &[String]) -> Result<()> {
    for lang in langs {
        if !VALID_LANGS.contains(&lang.as_str()) {
            return Err(anyhow::anyhow!(
                "Unknown language '{}'. Valid languages: {}",
                lang,
                VALID_LANGS.join(", ")
            ));
        }
    }
    Ok(())
}

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

#[cfg(test)]
mod file_command_tests {
    use super::*;

    fn files_target(command: &str) -> Target {
        Target {
            command: command.to_string(),
            depends_on: vec![],
            capabilities: HashSet::from([TargetCapability::FilesList]),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
            exclusive_resources: vec![],
        }
    }

    #[test]
    fn files_placeholder_expands_to_literal_arguments() {
        let file = PathBuf::from("tests/x;touch${IFS}pwned.js");
        let expanded = apply_files_to_command(
            &files_target("./capture {files}"),
            std::slice::from_ref(&file),
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            shell_words::split(&expanded).unwrap(),
            vec!["./capture".to_string(), file.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn files_placeholder_inside_shell_script_is_rejected() {
        let error = apply_files_to_command(
            &files_target("sh -c 'tool {files}'"),
            &[PathBuf::from("test.js")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("{files} must be a standalone command argument"));
    }

    #[test]
    fn files_placeholder_as_executable_is_rejected() {
        let error = apply_files_to_command(
            &files_target("{files} --flag"),
            &[PathBuf::from("tool")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be used as the executable"));
    }

    #[test]
    fn files_placeholder_as_interpreter_input_is_rejected() {
        let error = apply_files_to_command(
            &files_target("sh -c {files}"),
            &[PathBuf::from("touch pwned")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be expanded directly through a command interpreter"));
    }

    #[test]
    fn files_placeholder_cannot_hide_an_interpreter_behind_a_launcher() {
        let error = apply_files_to_command(
            &files_target("nice -n 5 /bin/sh -c {files}"),
            &[PathBuf::from("touch pwned")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be expanded directly through a command interpreter"));
    }
}
