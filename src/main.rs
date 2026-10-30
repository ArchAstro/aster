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

use aster::cli::affected_files::{
    apply_warnings_as_errors, plan_affected_commands, related_proxies, AffectedRequest,
};
use aster::cli::source_graph;
use aster::cli::{
    build_execution_output, build_execution_output_with_cache, check_reserved_target,
    expand_selection, output_json, parse_run_args, print_summary, select_projects, CacheCommands,
    CiCommands, Cli, Commands, GraphOutput, OutputMode, ProjectCommands, ProjectInfo,
    ResultsCommands, ServicesCommands, ServicesDaemonCommands, SrvCommands, SrvTarget, TlsCommands,
    WhyOutput, SKILLS_MARKDOWN,
};
use aster::config::{find_workspace_root, WorkspaceConfig};
use aster::discovery::{discover_projects, DiscoveredProject};
use aster::executor::logs::LogStore;
use aster::executor::{
    collect_selected_targets, collect_target_deps, compute_target_levels, install_signal_handler,
    parse_target_address, shutdown_signal, CommandOverride, Executor,
};
use aster::git::{
    affected_with_dependents, files_to_projects, find_lane, select_affected_lane,
    select_by_declared_inputs, AffectedDetector, AffectedIgnore,
};
use aster::graph::{build_graph, build_target_graph, find_cycle, format_path};
use aster::plugins::PluginRegistry;
use aster::test_results::detect_run;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Number of characters to show when displaying truncated cache hashes
const CACHE_HASH_DISPLAY_LENGTH: usize = 8;

fn main() -> ExitCode {
    if let Some(result) = aster::executor::admission::internal_invocation() {
        return match result {
            Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            Err(error) => {
                eprintln!("execution supervisor: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    match env::current_exe() {
        Ok(executable) => aster::executor::admission::enable(executable),
        Err(error) => {
            eprintln!("cannot locate Aster's execution supervisor: {error}");
            return ExitCode::FAILURE;
        }
    }
    if aster::dev::is_internal_serve_invocation() {
        return match aster::dev::serve_from_environment() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("daemon error: {error}");
                ExitCode::FAILURE
            }
        };
    }
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
    if let Some(jobs) = cli.jobs {
        env::set_var("ASTER_JOBS", jobs.to_string());
    }
    if cli.skills {
        return print_skills();
    }
    // `--cache-dir` wins over a pre-existing ASTER_CACHE_DIR. Export it before
    // any thread or child process starts so daemon children share one source
    // of truth. A relative flag is made absolute against cwd.
    if let Some(cache_dir) = cli.cache_dir.as_ref() {
        let absolute = if cache_dir.is_absolute() {
            cache_dir.clone()
        } else {
            env::current_dir()
                .context("Failed to get current directory")?
                .join(cache_dir)
        };
        env::set_var(aster::cache::CACHE_DIR_ENV, absolute);
    }
    let command = cli
        .command
        .take()
        .expect("clap requires either --skills or a subcommand");
    let output_mode = cli.output_mode();
    let full_logs = cli.full_logs();

    let cwd = env::current_dir().context("Failed to get current directory")?;

    if let Commands::ExecGoTest { selection, args } = &command {
        let code = aster::plugins::go::exec_go_test(selection.as_deref(), args)?;
        std::process::exit(code);
    }

    // Handle init and daemon lifecycle commands before requiring a workspace.
    if matches!(command, Commands::Init) {
        return handle_init(&cwd, cli.verbose);
    }
    if let Commands::Results { command } = &command {
        return handle_results(command, output_mode);
    }
    if let Commands::Srv { command } = &command {
        return handle_srv(command, output_mode);
    }
    // These talk to aster-srv and the user's config directory, not the
    // workspace, so they work outside one.
    if let Commands::Cache { command } = &command {
        match command {
            CacheCommands::Login {
                no_browser,
                loopback,
            } => {
                eprintln!("[aster] `aster cache login` is now `aster srv login`");
                return srv_login(*no_browser, *loopback, output_mode);
            }
            CacheCommands::Logout => {
                eprintln!("[aster] `aster cache logout` is now `aster srv logout`");
                return srv_logout();
            }
            CacheCommands::Stats { target } => {
                return print_srv_dataset(
                    &aster::test_results::read::Dataset::CacheSummary.path(),
                    target,
                );
            }
            CacheCommands::Clear { .. }
            | CacheCommands::Status { .. }
            | CacheCommands::Prune { .. } => {}
        }
    }
    if let Commands::Services {
        command: ServicesCommands::Daemon { command },
    } = &command
    {
        return match command {
            ServicesDaemonCommands::Stop => {
                let stopped = aster::dev::stop_all_bundles()?;
                if output_mode == OutputMode::Json {
                    output_json(&serde_json::json!({"stopped": stopped.len()}))?;
                } else if output_mode != OutputMode::Quiet {
                    println!(
                        "Stopped {} daemon-managed service bundle{}.",
                        stopped.len(),
                        if stopped.len() == 1 { "" } else { "s" }
                    );
                }
                Ok(())
            }
            ServicesDaemonCommands::Serve => {
                aster::dev::serve_from_environment().map_err(Into::into)
            }
        };
    }

    // Explicit numeric port cleanup is useful even outside an Aster workspace.
    // Named/default selection still loads the workspace configuration below.
    if let Commands::Services {
        command: ServicesCommands::KillPorts { ports, dry_run },
    } = &command
    {
        if !ports.is_empty() && ports.iter().all(|port| port.parse::<u16>().is_ok()) {
            let selected = aster::dev::resolve_port_selection(&HashMap::new(), ports)?;
            let options = aster::dev::KillPortsOptions { dry_run: *dry_run };
            return match find_workspace_root(&cwd) {
                Some(workspace_root) => {
                    aster::dev::kill_workspace_ports(&workspace_root, &selected, options)
                }
                None => aster::dev::kill_ports(&selected, options),
            };
        }
    }

    // For all other commands, require a workspace
    let workspace_root = find_workspace_root(&cwd).context(
        "Not in an aster workspace (no aster.toml or .git found). Run 'aster init' to create one.",
    )?;

    // Daemon inspection and worktree shutdown need configuration for stable
    // reporting, but do not require project/plugin discovery.
    if let Commands::Services { command } = &command {
        match command {
            ServicesCommands::List => {
                let workspace_config = WorkspaceConfig::load(&workspace_root)?;
                let bundles = aster::dev::list_workspace_bundles(&workspace_root)?;
                let report = aster::dev::service_bundles_report(&workspace_root, bundles)?;
                if output_mode == OutputMode::Json {
                    output_json(&report)?;
                } else if output_mode != OutputMode::Quiet {
                    print!(
                        "{}",
                        aster::dev::format_service_bundles(&report, &workspace_config.dev)
                    );
                }
                return Ok(());
            }
            ServicesCommands::Down { group } => {
                let stopped =
                    aster::dev::stop_workspace_bundles(&workspace_root, group.as_deref())?;
                if output_mode == OutputMode::Json {
                    output_json(&serde_json::json!({"stopped": stopped.len()}))?;
                } else if output_mode != OutputMode::Quiet {
                    println!(
                        "Stopped {} daemon-managed service bundle{} in this worktree.",
                        stopped.len(),
                        if stopped.len() == 1 { "" } else { "s" }
                    );
                }
                return Ok(());
            }
            ServicesCommands::Up {
                group,
                no_watch,
                no_ui,
                dry_run,
                daemon,
                proxy,
            } => {
                // Configuration is loaded here to preserve normal workspace
                // diagnostics; full discovery and validation happen in the child.
                let workspace_config = WorkspaceConfig::load(&workspace_root)?;
                let daemon_supervisor = env::var_os(aster::dev::SUPERVISOR_ENV).is_some()
                    && env::var_os("ASTER_INTERNAL_DAEMON_READY_SOCKET").is_some();
                let use_daemon =
                    !daemon_supervisor && (*daemon || (workspace_config.dev.daemon && !dry_run));
                if use_daemon {
                    let mut options = aster::dev::LaunchOptions::new(&workspace_root)?;
                    options.group.clone_from(group);
                    options.watch = !no_watch;
                    options.use_cache = !cli.no_cache;
                    options.proxy = *proxy;
                    let result = aster::dev::launch_bundle(options)?;
                    let attach_ui = workspace_config.dev.daemon
                        && !daemon
                        && !no_ui
                        && matches!(output_mode, OutputMode::Normal | OutputMode::Verbose);
                    if attach_ui {
                        #[cfg(unix)]
                        {
                            let socket =
                                aster::dev::attach_bundle(&workspace_root, group.as_deref())?;
                            aster::dev::attach_dashboard(&workspace_root, &socket)?;
                        }
                        #[cfg(not(unix))]
                        unreachable!("daemon launch rejects unsupported platforms");
                    } else if output_mode == OutputMode::Json {
                        output_json(&result)?;
                    } else if output_mode != OutputMode::Quiet {
                        let action = match result.status {
                            aster::dev::LaunchStatus::Started => "Started",
                            aster::dev::LaunchStatus::AlreadyRunning => "Already running",
                        };
                        let group = result.bundle.display_group.as_deref().unwrap_or("default");
                        println!(
                            "{action} service bundle '{group}' (supervisor {}).",
                            result.bundle.supervisor_pid
                        );
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
    }

    // Reading existing logs does not require project discovery or graph validation.
    if let Commands::Services {
        command: ServicesCommands::Logs { service },
    } = &command
    {
        let workspace_config = WorkspaceConfig::load(&workspace_root)?;
        return aster::dev::show_service_logs(&workspace_root, &workspace_config.dev, service);
    }

    // Allocation metadata is independent of project discovery and remains
    // useful after a supervisor crash leaves service listeners behind.
    if let Commands::Services {
        command: ServicesCommands::Ports,
    } = &command
    {
        let workspace_config = WorkspaceConfig::load(&workspace_root)?;
        let report = aster::dev::workspace_ports_report(&workspace_root, &workspace_config.dev)?;
        if output_mode == OutputMode::Json {
            output_json(&report)?;
        } else if output_mode != OutputMode::Quiet {
            print!("{}", aster::dev::format_workspace_ports(&report));
        }
        return Ok(());
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

    // `aster check` reports a configuration it cannot load with its own
    // exit status, so it discovers projects itself.
    if let Commands::Check {
        strict,
        rule,
        entry,
    } = &command
    {
        let code = match handle_check(
            &workspace_root,
            *strict,
            rule,
            entry,
            output_mode,
            !cli.no_cache,
        ) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error:#}");
                2
            }
        };
        std::process::exit(code);
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
        Commands::Init | Commands::ExecGoTest { .. } => unreachable!("handled above"),
        Commands::Check { .. } => unreachable!("check handled above"),
        Commands::Results { .. } => unreachable!("results handled above"),
        Commands::Srv { .. } => unreachable!("srv handled above"),
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
        Commands::Graph {
            source: true,
            commit,
            dir,
            ext,
            ..
        } => {
            let detector = AffectedDetector::new(&workspace_root)
                .context("Failed to initialize git repository")?;
            let mut changes = detector
                .changes_in(commit.as_deref())
                .context("Failed to read the changed lines")?;
            source_graph::filter_changes(&mut changes, dir.as_deref(), &ext);
            changes.sort_by(|a, b| a.path.cmp(&b.path));
            let graph = build_graph(&projects)?;
            let facts_cache =
                (!cli.no_cache).then(|| aster::cache::resolve_cache_dir(&workspace_root));
            let analysis = aster::related::analyse(
                &workspace_root,
                &projects,
                &graph,
                &changes,
                facts_cache.as_deref(),
            );
            let range = source_graph::describe_range(commit.as_deref());
            if output_mode == OutputMode::Json {
                let value = source_graph::render_json(&analysis, &range);
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print!("{}", source_graph::render_text(&analysis, &range));
            }
        }
        Commands::Graph { target, .. } => {
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

                let workspace_inputs = aster::graph::workspace_inputs(&projects, &nodes);
                let selected_by = aster::graph::selected_by(&projects, &nodes);
                let output = GraphOutput {
                    nodes,
                    edges,
                    workspace_inputs,
                    selected_by,
                };
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
                        let inputs =
                            aster::graph::workspace_inputs(&projects, std::slice::from_ref(&addr));
                        for glob in inputs.get(&addr).into_iter().flatten() {
                            println!("  {} {}", style("reads").dim(), style(glob).dim());
                        }
                        let selectors =
                            aster::graph::selected_by(&projects, std::slice::from_ref(&addr));
                        for selector in selectors.get(&addr).into_iter().flatten() {
                            println!("  {} {}", style("selected by").dim(), style(selector).dim());
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
            lane,
            dry_run,
            only_affected_files,
            related,
            warnings_as_errors,
            lang,
            all,
            matrix: matrix_rows,
        } => {
            validate_lang_filter(&lang)?;
            // --matrix plans every lane and prints JSON only.
            let dry_run = dry_run || matrix_rows;
            let output_mode = if matrix_rows {
                OutputMode::Json
            } else {
                output_mode
            };
            // --all selects every project and runs it in full.
            let related = related && !all;
            let dependents = dependents && !all;

            // Create affected detector from workspace (requires git)
            let detector = AffectedDetector::new(&workspace_root)
                .context("Not in a git repository. The 'affected' command requires git.")?;

            // Get changed files
            let changed_files = if all {
                HashSet::new()
            } else {
                detector
                    .all_affected_files(&base, head.as_deref())
                    .with_context(|| {
                        format!(
                            "Failed to detect changed files between '{}' and '{}'",
                            base,
                            head.as_deref().unwrap_or("HEAD + uncommitted")
                        )
                    })?
            };

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
            let directly_affected = if all {
                projects
                    .iter()
                    .map(|p| format!("//{}", p.relative_path.display()))
                    .collect()
            } else {
                files_to_projects(&changed_files, &projects)
            };

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

            // Under --related the source graph decides which projects the
            // change reaches and which of their tests observe it.
            let related_analysis = if related {
                let mut changes = detector
                    .changes(&base, head.as_deref())
                    .context("Failed to read the changed lines")?;
                changes.retain(|change| changed_files.contains(&change.path));
                for file in &changed_files {
                    // No hunks known: the whole file counts as changed.
                    if !changes.iter().any(|change| &change.path == file) {
                        changes.push(aster::related::Change {
                            path: file.clone(),
                            deleted: !workspace_root.join(file).exists(),
                            ..Default::default()
                        });
                    }
                }
                changes.sort_by(|a, b| a.path.cmp(&b.path));
                let facts_cache =
                    (!cli.no_cache).then(|| aster::cache::resolve_cache_dir(&workspace_root));
                let analysis = aster::related::analyse(
                    &workspace_root,
                    &projects,
                    &graph,
                    &changes,
                    facts_cache.as_deref(),
                );
                if output_mode == OutputMode::Verbose {
                    eprintln!(
                        "[aster] Analysed {} source files, {} from cache ({})",
                        analysis.files_analysed, analysis.facts_reused, analysis.timings
                    );
                }
                Some(analysis)
            } else {
                None
            };

            // Expand with dependents if requested
            let affected_addrs = if let Some(analysis) = &related_analysis {
                let reached: HashSet<String> = analysis
                    .projects
                    .iter()
                    .filter(|(_, outcome)| outcome.affected())
                    .map(|(addr, _)| addr.clone())
                    .collect();
                // Shard and wrapper projects are added per lane below, for
                // the target each lane runs.
                reached
            } else if dependents {
                affected_with_dependents(directly_affected, &graph)
            } else {
                directly_affected
            };

            #[derive(serde::Serialize)]
            struct DryRunTarget {
                address: String,
                reason: String,
                files: Vec<String>,
                /// Commands replacing the target's own command.
                #[serde(skip_serializing_if = "Vec::is_empty")]
                commands: Vec<String>,
                /// How the target's command was chosen.
                #[serde(skip_serializing_if = "Vec::is_empty")]
                selection: Vec<String>,
            }
            #[derive(serde::Serialize)]
            struct LaneRow {
                lane: String,
                name: String,
                target: String,
                toolchains: Vec<String>,
                #[serde(flatten)]
                matrix: toml::Table,
            }
            #[derive(serde::Serialize)]
            struct DryRunOutput {
                target: String,
                base: String,
                head: Option<String>,
                targets: Vec<DryRunTarget>,
                count: usize,
                /// Requested targets dropped by --only-affected-files.
                #[serde(skip_serializing_if = "Vec::is_empty")]
                skipped: Vec<String>,
                /// Plugins of every project the plan runs a target in, plus
                /// the lane's declared toolchains: what a runner must install.
                toolchains: Vec<String>,
            }
            // Several lanes are planned from the one analysis above; running
            // them is one invocation per lane, each on its own runner.
            let lane = if matrix_rows && lane.is_empty() {
                let mut configured: Vec<String> =
                    workspace_config.affected.lanes.keys().cloned().collect();
                if configured.is_empty() {
                    // An empty matrix would read as "nothing to run".
                    return Err(anyhow::anyhow!(
                        "--matrix needs lanes: configure [affected.lanes.<name>] in the root aster.toml"
                    ));
                }
                configured.sort();
                configured
            } else {
                lane
            };
            let several_lanes = lane.len() > 1 || matrix_rows;
            if several_lanes && !dry_run {
                return Err(anyhow::anyhow!(
                    "Several lanes can be planned together but not run together: \
                     add --dry-run, or pass one --lane"
                ));
            }
            let reached = affected_addrs;
            let lanes: Vec<Option<&str>> = if lane.is_empty() {
                vec![None]
            } else {
                lane.iter().map(|name| Some(name.as_str())).collect()
            };
            let mut lane_plans = serde_json::Map::new();
            let mut lane_rows: Vec<LaneRow> = Vec::new();
            for lane in lanes {
                let lane_label = lane.unwrap_or_default();
                let lane_config = lane
                    .map(|name| find_lane(name, &workspace_config.affected))
                    .transpose()?;
                // A lane can run its own target instead of the requested one.
                let target = lane_config
                    .and_then(|config| config.target.clone())
                    .unwrap_or_else(|| target.clone());
                let lane_toolchains = lane_config
                    .map(|config| config.toolchains.clone())
                    .unwrap_or_default();
                let mut affected_addrs = reached.clone();

                // A changed file the target declares as a workspace input
                // selects its project, which then runs the target in full.
                let declared_inputs =
                    select_by_declared_inputs(&target, &changed_files, &projects)?;
                let declared_addrs: HashSet<String> = declared_inputs.keys().cloned().collect();
                affected_addrs.extend(declared_addrs.iter().cloned());
                let mut directly_affected_addrs = directly_affected_addrs.clone();
                directly_affected_addrs.extend(declared_addrs.iter().cloned());
                // Shard and wrapper projects follow the project whose tests
                // they run.
                if let Some(analysis) = &related_analysis {
                    affected_addrs.extend(related_proxies(
                        &projects,
                        &target,
                        analysis,
                        &PluginRegistry::with_all_plugins(),
                        &workspace_config.affected.related.wrappers,
                    ));
                }

                // Lane selection applies only to primary projects, after optional
                // dependent expansion and before target dependency closure.
                if let Some(lane_name) = lane {
                    affected_addrs = select_affected_lane(
                        lane_name,
                        &workspace_config.affected,
                        affected_addrs,
                        &projects,
                    )?;
                    if output_mode == OutputMode::Verbose {
                        eprintln!("[aster] Affected lane '{lane_name}': {affected_addrs:?}");
                    }
                }
                // A lane, or --all, runs the target only where it is defined.
                // Without either, an affected project that lacks the target is
                // still reported, as before.
                if lane_config.is_some() || all {
                    affected_addrs.retain(|addr| {
                        projects.iter().any(|p| {
                            format!("//{}", p.relative_path.display()) == *addr
                                && p.targets.contains_key(&target)
                        })
                    });
                }

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
                    if several_lanes {
                        if output_mode == OutputMode::Json {
                            let empty = DryRunOutput {
                                target: target.clone(),
                                base: base.clone(),
                                head: head.clone(),
                                targets: Vec::new(),
                                count: 0,
                                skipped: Vec::new(),
                                toolchains: Vec::new(),
                            };
                            lane_plans
                                .insert(lane_label.to_string(), serde_json::to_value(&empty)?);
                        } else if output_mode != OutputMode::Quiet {
                            println!("Lane {lane_label}: no projects affected");
                            println!();
                        }
                        continue;
                    }
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

                if several_lanes
                    && output_mode != OutputMode::Quiet
                    && output_mode != OutputMode::Json
                {
                    println!("Lane {lane_label}:");
                }
                if output_mode == OutputMode::Verbose {
                    eprintln!(
                        "[aster] Running '{}' on {} affected projects",
                        target,
                        ordered.len()
                    );
                }

                // Machine-readable output must contain JSON only, including dry runs.
                if output_mode != OutputMode::Quiet && output_mode != OutputMode::Json {
                    println!(
                        "Affected projects ({}):",
                        if lane.is_some() {
                            "selected by lane"
                        } else if related {
                            "reached by the change"
                        } else if dependents {
                            "including dependents"
                        } else {
                            "directly affected only"
                        }
                    );
                    for p in &ordered {
                        println!("  //{}", p.relative_path.display());
                    }
                }

                // Narrow targets to changed files and apply warnings-as-errors.
                // Under --dependents, a project whose dependency changed cannot be
                // narrowed to its own files.
                let command_plan = if only_affected_files || related || warnings_as_errors {
                    let registry = PluginRegistry::with_all_plugins();
                    // Under --dependents a project answers for a changed
                    // project it depends on, which its own files do not
                    // describe. Without it the run is about each project's
                    // own change.
                    let dependency_changed: HashSet<String> = if only_affected_files && dependents {
                        directly_affected_addrs
                            .iter()
                            .flat_map(|addr| {
                                let mut reached =
                                    affected_with_dependents(HashSet::from([addr.clone()]), &graph);
                                reached.remove(addr);
                                reached
                            })
                            .filter(|addr| primary_addrs.contains(addr))
                            .collect()
                    } else {
                        HashSet::new()
                    };
                    let mut changed_list: Vec<PathBuf> = changed_files.iter().cloned().collect();
                    changed_list.sort();
                    Some(plan_affected_commands(
                        &AffectedRequest {
                            target: &target,
                            projects: &ordered,
                            changed_files: &changed_list,
                            primary: &primary_addrs,
                            dependency_changed: &dependency_changed,
                            only_affected_files,
                            warnings_as_errors,
                            related: related_analysis.as_ref(),
                            all_projects: &projects,
                            declared_inputs: &declared_addrs,
                            related_wrappers: &workspace_config.affected.related.wrappers,
                        },
                        &registry,
                    )?)
                } else {
                    None
                };
                let primary_addrs = command_plan
                    .as_ref()
                    .map(|plan| plan.primary.clone())
                    .unwrap_or(primary_addrs);
                let no_notes: HashMap<String, Vec<String>> = HashMap::new();
                let target_notes = command_plan
                    .as_ref()
                    .map(|plan| &plan.notes)
                    .unwrap_or(&no_notes);

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
                    collect_selected_targets(&all_project_map, &mut targets_to_run);

                    // A target that joined through `selected_by` says whose run.
                    let joined_runs = |target_addr: &str| -> Vec<String> {
                        parse_target_address(target_addr)
                            .and_then(|(project_addr, name)| {
                                all_project_map.get(&project_addr)?.targets.get(&name)
                            })
                            .map(|def| {
                                def.selected_by
                                    .iter()
                                    .filter(|selector| targets_to_run.contains(*selector))
                                    .map(|selector| {
                                        format!("selected_by: joins the run of {selector}")
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };

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
                    for (project_addr, files) in &declared_inputs {
                        files_per_project
                            .entry(project_addr.clone())
                            .or_default()
                            .extend(files.iter().map(|f| f.to_string_lossy().to_string()));
                    }

                    let mut toolchains: Vec<String> = targets_to_run
                        .iter()
                        .filter_map(|addr| parse_target_address(addr))
                        .filter_map(|(project_addr, _)| all_project_map.get(&project_addr))
                        .map(|project| project.plugin_name.clone())
                        .chain(lane_toolchains.iter().cloned())
                        .collect();
                    toolchains.sort();
                    toolchains.dedup();

                    // Categorize each target's rationale
                    let rationale_for = |target_addr: &str| -> &'static str {
                        if let Some((project_addr, target_name)) = parse_target_address(target_addr)
                        {
                            if target_name == target && primary_addrs.contains(&project_addr) {
                                // Only lane-selected requested targets are primary work.
                                if directly_affected_addrs.contains(&project_addr) {
                                    return "affected";
                                }
                                return "dependent";
                            }
                        }
                        "target dependency"
                    };

                    if output_mode == OutputMode::Json {
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
                                let commands = match command_plan
                                    .as_ref()
                                    .and_then(|plan| plan.overrides.get(target_addr))
                                {
                                    Some(CommandOverride::Run(commands)) => commands.clone(),
                                    _ => vec![],
                                };
                                let mut selection =
                                    target_notes.get(target_addr).cloned().unwrap_or_default();
                                selection.extend(joined_runs(target_addr));
                                all_targets.push(DryRunTarget {
                                    address: target_addr.clone(),
                                    reason,
                                    files,
                                    commands,
                                    selection,
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
                            skipped: command_plan
                                .as_ref()
                                .map(|plan| plan.skipped.clone())
                                .unwrap_or_default(),
                            toolchains,
                        };
                        if let (true, Some(config)) = (!primary_addrs.is_empty(), lane_config) {
                            lane_rows.push(LaneRow {
                                lane: lane_label.to_string(),
                                name: config
                                    .name
                                    .clone()
                                    .unwrap_or_else(|| lane_label.to_string()),
                                target: target.clone(),
                                toolchains: output.toolchains.clone(),
                                matrix: config.matrix.clone(),
                            });
                        }
                        if several_lanes {
                            lane_plans
                                .insert(lane_label.to_string(), serde_json::to_value(&output)?);
                        } else {
                            output_json(&output)?;
                        }
                    } else if output_mode != OutputMode::Quiet {
                        // Count targets by category
                        let dep_count = targets_to_run
                            .iter()
                            .filter(|t| rationale_for(t) == "target dependency")
                            .count();

                        if let Some(plan) = &command_plan {
                            if !plan.skipped.is_empty() {
                                println!();
                                if related {
                                    println!("Skipped by --related (no test reaches the change):");
                                } else {
                                    println!(
                                    "Skipped by --only-affected-files (no relevant changed files):"
                                );
                                }
                                for addr in &plan.skipped {
                                    println!("  {addr}");
                                    for line in target_notes.get(addr).into_iter().flatten() {
                                        println!("    > {line}");
                                    }
                                }
                            }
                        }

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
                                    if let Some((project_addr, _)) =
                                        parse_target_address(target_addr)
                                    {
                                        if let Some(files) = files_per_project.get(&project_addr) {
                                            for file in files {
                                                println!("    - {file}");
                                            }
                                        }
                                    }
                                }
                                for line in target_notes.get(target_addr).into_iter().flatten() {
                                    println!("    > {line}");
                                }
                            }
                        }
                        if several_lanes {
                            println!();
                        }
                    }
                    if several_lanes {
                        continue;
                    }
                    return Ok(());
                }

                // Execute target on projects
                // Pass ALL projects so executor can resolve target-level dependencies
                // (e.g., //app:build depends on //lib:build). Only primary (affected)
                // projects will run the requested target.
                let test_results = cli.test_results();
                let run_record = test_results.as_ref().map(|_| {
                    detect_run(&workspace_root, &target, lane, &|name| env::var(name).ok())
                });
                let executor = Executor::with_all_options(
                    &workspace_root,
                    output_mode,
                    full_logs,
                    !cli.no_cache,
                )
                .with_test_capture(test_results.is_some())
                .with_retry_failed(cli.retry_failed())
                .with_no_replay(cli.no_replay)
                .with_quarantine(quarantine_for(
                    &workspace_root,
                    test_results.is_some() || cli.retry_failed() > 0,
                )?);
                let all_project_refs: Vec<_> = projects.iter().collect();

                let results = if let Some(plan) = &command_plan {
                    if output_mode == OutputMode::Verbose {
                        let mut addresses: Vec<&String> = plan.notes.keys().collect();
                        addresses.sort();
                        for addr in addresses {
                            for line in &plan.notes[addr] {
                                eprintln!("[aster] {addr}: {line}");
                            }
                        }
                        for addr in &plan.skipped {
                            eprintln!(
                            "[aster] Skipping {addr}: no matching files for --only-affected-files"
                        );
                        }
                        if !plan.overrides.is_empty() {
                            eprintln!(
                                "[aster] Using modified commands for {} targets",
                                plan.overrides.len()
                            );
                        }
                    }
                    executor.execute_with_command_overrides(
                        &target,
                        &all_project_refs,
                        &plan.overrides,
                        Some(&plan.primary),
                    )
                } else {
                    executor.execute(&target, &all_project_refs, &graph, Some(&primary_addrs))
                };
                write_test_results(test_results, run_record, &results);

                // Output results based on mode
                if output_mode == OutputMode::Json {
                    let output = build_execution_output_with_cache(
                        &results,
                        aster::cache::current_gha().map(|backend| backend.summary()),
                    );
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
            if matrix_rows {
                output_json(&lane_rows)?;
            } else if several_lanes && output_mode == OutputMode::Json {
                output_json(&serde_json::json!({ "lanes": lane_plans }))?;
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
            let test_results = cli.test_results();
            let run_record = test_results
                .as_ref()
                .map(|_| detect_run(&workspace_root, "run", None, &|name| env::var(name).ok()));
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache)
                    .with_test_capture(test_results.is_some())
                    .with_retry_failed(cli.retry_failed())
                    .with_no_replay(cli.no_replay)
                    .with_quarantine(quarantine_for(
                        &workspace_root,
                        test_results.is_some() || cli.retry_failed() > 0,
                    )?);
            let results = executor.execute_targets(&primary_targets, &all_project_refs, no_deps);
            write_test_results(test_results, run_record, &results);

            // Output results based on mode
            if output_mode == OutputMode::Json {
                let output = build_execution_output_with_cache(
                    &results,
                    aster::cache::current_gha().map(|backend| backend.summary()),
                );
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
        Commands::Ci { command } => {
            let CiCommands::CachePlan {
                projects: addresses,
                all,
                github_output,
            } = command;
            let mut included = Vec::new();
            let mut exclusions = Vec::new();
            for address in &addresses {
                if let Some(excluded) = address.strip_prefix('-') {
                    exclusions.push(excluded.to_string());
                } else {
                    included.push(address.clone());
                }
            }
            // No addresses means every discovered project. `aster run`
            // instead falls back to the project containing cwd.
            let select_all = all || included.is_empty();
            let graph = build_graph(&projects)?;
            let args = aster::cli::RunArgs {
                all: select_all,
                projects: if select_all { Vec::new() } else { included },
                exclusions,
                ..aster::cli::RunArgs::default()
            };
            let selected = select_projects(&args, &graph, &projects, &cwd, &workspace_root)
                .map_err(|error| anyhow::anyhow!(error))?;
            let runtime = aster::cache::RuntimeInputCache::new(&workspace_root);
            let plan = aster::cli::cache_plan::plan_native_caches(
                &selected,
                &registry,
                &workspace_root,
                &runtime,
            )?;
            aster::cli::cache_plan::render_cache_plan(&plan, output_mode, github_output)?;
        }
        Commands::Cache { command } => {
            use aster::cache::{resolve_cache_dir, ArtifactCache};
            use aster::cli::CacheCommands;

            let artifact_cache =
                ArtifactCache::new(resolve_cache_dir(&workspace_root), &workspace_root);

            match command {
                CacheCommands::Clear { target } => {
                    if let Some(pattern) = target {
                        let removed = if pattern.contains(':') {
                            let before = artifact_cache.status(Some(&pattern))?.targets.len();
                            artifact_cache.remove(&pattern)?;
                            before
                        } else {
                            let before = artifact_cache.list_project_entries(&pattern)?.len();
                            artifact_cache.invalidate_project(&pattern)?;
                            before
                        };
                        if output_mode != OutputMode::Quiet {
                            println!("Cleared {removed} cache entries matching {pattern}");
                        }
                    } else {
                        artifact_cache.clear()?;
                        if output_mode != OutputMode::Quiet {
                            println!("Cache cleared");
                        }
                    }
                }
                CacheCommands::Status { target, inputs } => {
                    if inputs {
                        let addr = target.ok_or_else(|| {
                            anyhow::anyhow!(
                                "cache status --inputs requires a target address, for example //app:build"
                            )
                        })?;
                        print_cache_inputs(
                            &addr,
                            &workspace_root,
                            &projects,
                            &registry,
                            output_mode,
                        )?;
                    } else {
                        let status = artifact_cache.status(target.as_deref())?;
                        print_cache_status(&status, target.as_deref(), output_mode)?;
                    }
                }
                CacheCommands::Login { .. }
                | CacheCommands::Logout
                | CacheCommands::Stats { .. } => {
                    unreachable!("handled before workspace discovery")
                }
                CacheCommands::Prune { max_size, max_age } => {
                    use aster::cache::{prune_cache, PruneLimits};
                    use aster::config::WorkspaceConfig;

                    let workspace = WorkspaceConfig::load(&workspace_root)?;
                    let limits = PruneLimits::from_config(
                        workspace.cache.max_size.as_deref(),
                        workspace.cache.max_age.as_deref(),
                    )?
                    .with_flags(max_size.as_deref(), max_age.as_deref())?;
                    let stats = prune_cache(&workspace_root, limits)?;
                    if output_mode == OutputMode::Json {
                        output_json(&stats)?;
                    } else if output_mode != OutputMode::Quiet {
                        println!(
                            "Pruned {} manifests and {} blobs ({} bytes remaining)",
                            stats.manifests_removed, stats.blobs_removed, stats.bytes_remaining
                        );
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
                daemon: false,
                proxy,
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
                    proxy,
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
                    aster::dev::resolve_static_dev_ports(&workspace_root, &workspace_config.dev)?;
                let selected = aster::dev::resolve_workspace_port_selection(
                    &workspace_root,
                    &configured,
                    &ports,
                )?;
                aster::dev::kill_workspace_ports(
                    &workspace_root,
                    &selected,
                    aster::dev::KillPortsOptions { dry_run },
                )?;
            }
            ServicesCommands::Up { daemon: true, .. } => {
                unreachable!("daemon service up handled before discovery")
            }
            ServicesCommands::List | ServicesCommands::Down { .. } => {
                unreachable!("daemon worktree commands handled before discovery")
            }
            ServicesCommands::Daemon { .. } => {
                unreachable!("daemon lifecycle handled before workspace discovery")
            }
            ServicesCommands::Logs { .. } => unreachable!("service logs handled before discovery"),
            ServicesCommands::Ports => unreachable!("service ports handled before discovery"),
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
            if let Some(jobs) = &run_args.jobs {
                let jobs = jobs
                    .parse::<std::num::NonZeroUsize>()
                    .context("--jobs must be a positive integer")?;
                env::set_var("ASTER_JOBS", jobs.to_string());
            }
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
            let test_results = cli.test_results();
            let run_record = test_results.as_ref().map(|_| {
                detect_run(&workspace_root, &run_args.target, None, &|name| {
                    env::var(name).ok()
                })
            });
            let executor =
                Executor::with_all_options(&workspace_root, output_mode, full_logs, !cli.no_cache)
                    .with_test_capture(test_results.is_some())
                    .with_retry_failed(cli.retry_failed())
                    .with_no_replay(cli.no_replay)
                    .with_quarantine(quarantine_for(
                        &workspace_root,
                        test_results.is_some() || cli.retry_failed() > 0,
                    )?);

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
                let mut command_overrides: HashMap<String, CommandOverride> = HashMap::new();

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
                            command_overrides
                                .insert(target_addr, CommandOverride::Run(vec![modified_cmd]));
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
            write_test_results(test_results, run_record, &results);

            // Output results based on mode
            if output_mode == OutputMode::Json {
                let output = build_execution_output_with_cache(
                    &results,
                    aster::cache::current_gha().map(|backend| backend.summary()),
                );
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

/// Write the run's test results when recording is on. A file that cannot be
/// written is a warning: recording never fails the build.
fn write_test_results(
    destination: Option<Option<PathBuf>>,
    run: Option<aster_results_model::RunRecord>,
    results: &[aster::executor::ExecutionResult],
) {
    let (Some(destination), Some(run)) = (destination, run) else {
        return;
    };
    let path = destination.unwrap_or_else(|| aster::test_results::default_path(&run.run_id));
    let records = aster::test_results::records(run, results);
    let tests = records
        .iter()
        .filter(|record| matches!(record, aster_results_model::Record::Test(_)))
        .count();
    match aster::test_results::write(&path, &records) {
        Ok(()) => eprintln!(
            "[aster] Test results ({tests} test{}) written to {}",
            if tests == 1 { "" } else { "s" },
            path.display()
        ),
        Err(error) => {
            eprintln!("[aster] Warning: {error:#}");
            return;
        }
    }
    if let Some(url) = srv_url(None) {
        upload_test_results(&url, &path, OutputMode::Normal);
    }
    github_test_summary(&path);
}

/// `aster results ...`.
/// Run `aster check`; the exit status, or an error for a configuration
/// that cannot be loaded.
fn handle_check(
    workspace_root: &std::path::Path,
    strict: bool,
    rules: &[String],
    entries: &[String],
    output_mode: OutputMode,
    use_cache: bool,
) -> Result<i32> {
    let registry = PluginRegistry::with_all_plugins();
    let config = WorkspaceConfig::load(workspace_root)?;
    let projects =
        discover_projects(workspace_root, &registry).context("Failed to discover projects")?;
    let options = aster::check::Options {
        rules: rules.to_vec(),
        entries: entries.to_vec(),
        cache_dir: use_cache.then(|| aster::cache::resolve_cache_dir(workspace_root)),
    };
    let report = aster::check::run(workspace_root, &projects, &registry, &config, &options)?;
    if output_mode == OutputMode::Json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render_text());
        if output_mode == OutputMode::Verbose {
            eprintln!(
                "[aster] Checked {} source files, {} from cache, in {} ms",
                report.stats.source_files, report.stats.facts_reused, report.stats.elapsed_ms
            );
        }
    }
    Ok(report.exit_code(strict))
}

fn handle_results(command: &ResultsCommands, output_mode: OutputMode) -> Result<()> {
    match command {
        ResultsCommands::Upload { path, url, strict } => {
            let url = srv_url(url.as_deref())
                .ok_or_else(|| anyhow::anyhow!("give --url or set ASTER_SRV_URL"))?;
            if upload_test_results(&url, path, output_mode) || !strict {
                Ok(())
            } else {
                Err(anyhow::anyhow!("the test results were not uploaded"))
            }
        }
        ResultsCommands::Summary { path, url, github } => aster::test_results::summary::run(
            &aster::test_results::summary::Options {
                path,
                url: srv_url(url.as_deref()),
                github: *github,
                flaky_warnings: true,
            },
            &|name| env::var(name).ok(),
            &mut io::stdout().lock(),
        ),
        ResultsCommands::Failures {
            github_run,
            attempt,
            url,
        } => {
            use aster::test_results::{client::Server, upload::Credentials};
            let url = srv_url(url.as_deref())
                .ok_or_else(|| anyhow::anyhow!("give --url or set ASTER_SRV_URL"))?;
            let env = |name: &str| env::var(name).ok();
            let credentials = Credentials::from_env(&env).ok_or_else(|| {
                anyhow::anyhow!(
                    "no credentials: in GitHub Actions grant `id-token: write`; \
                     elsewhere set ASTER_SRV_TOKEN"
                )
            })?;
            let proxy_token = aster::test_results::upload::proxy_token_from_env(&env);
            let report = Server::connect(&url, &credentials, proxy_token)?
                .failures_for_github_run(github_run, *attempt)?
                .ok_or_else(|| {
                    anyhow::anyhow!("aster-srv has no results for GitHub run {github_run}")
                })?;
            println!("{report}");
            Ok(())
        }
        ResultsCommands::Flaky { target } => {
            print_srv_dataset(&aster::test_results::read::Dataset::Flaky.path(), target)
        }
        ResultsCommands::Broken { target } => {
            print_srv_dataset(&aster::test_results::read::Dataset::Broken.path(), target)
        }
        ResultsCommands::Quarantine { target } => print_srv_dataset(
            &aster::test_results::read::Dataset::Quarantine.path(),
            target,
        ),
        ResultsCommands::Clusters { target } => {
            print_srv_dataset(&aster::test_results::read::Dataset::Clusters.path(), target)
        }
        ResultsCommands::Test { id, target } => print_srv_dataset(
            &aster::test_results::read::Dataset::Test(id.clone()).path(),
            target,
        ),
        ResultsCommands::Durations { url } => {
            use aster::test_results::balance;
            let url = srv_url(url.as_deref())
                .ok_or_else(|| anyhow::anyhow!("give --url or set ASTER_SRV_URL"))?;
            let cwd = env::current_dir().context("Failed to get current directory")?;
            let workspace_root = find_workspace_root(&cwd)
                .ok_or_else(|| anyhow::anyhow!("not in an aster workspace"))?;
            let registry = PluginRegistry::with_all_plugins();
            let projects = discover_projects(&workspace_root, &registry)
                .context("Failed to discover projects")?;
            let project_map: HashMap<String, &DiscoveredProject> = projects
                .iter()
                .map(|project| (project_address(project), project))
                .collect();
            let suites = aster::executor::runner::partitioned_suites(&workspace_root, &project_map);
            if suites.is_empty() {
                anyhow::bail!("no partitioned `mix test --partitions` suites in this workspace");
            }
            let day = chrono::Utc::now().date_naive();
            let file = balance::fetch_all(&url, &suites, day, &|name| env::var(name).ok())?;
            balance::write(&workspace_root, &file)?;
            if output_mode != OutputMode::Quiet {
                for (dir, suite) in &file.suites {
                    eprintln!(
                        "[aster] {dir}: {} files timed from {} partitions",
                        suite.files.len(),
                        suite.targets.len()
                    );
                }
                eprintln!(
                    "[aster] Wrote {}",
                    workspace_root.join(balance::FILE).display()
                );
            }
            Ok(())
        }
    }
}

/// The aster-srv URL login, logout and whoami use: `--url`, else
/// `$ASTER_SRV_URL`.
fn srv_login_url(url: Option<&str>, verb: &str) -> Result<String> {
    srv_url(url).ok_or_else(|| {
        anyhow::anyhow!(
            "set {} to your aster-srv URL, then run aster srv {verb} again",
            aster::cache::ASTER_SRV_URL_ENV
        )
    })
}

/// `aster srv login`.
fn srv_login(no_browser: bool, loopback: bool, output_mode: OutputMode) -> Result<()> {
    let server = srv_login_url(None, "login")?;
    let options = aster::cache::LoginOptions {
        mode: match (loopback, no_browser) {
            (true, _) => aster::cache::LoginMode::Loopback,
            (false, true) => aster::cache::LoginMode::Copy,
            (false, false) => aster::cache::LoginMode::Auto,
        },
        open_browser: !no_browser,
        timeout: aster::cache::LOGIN_TIMEOUT,
    };
    let quiet = output_mode == OutputMode::Quiet;
    let signed_in = aster::cache::login(
        &server,
        &options,
        &mut |announcement| {
            use aster::cache::{CodeReason, Flow};
            // A pasted code cannot work without the URL and the prompt,
            // so those print even when quiet.
            let copy = matches!(announcement.flow, Flow::Code(_));
            if quiet && !copy {
                return;
            }
            match announcement.flow {
                Flow::OpenedBrowser => {
                    println!("Opening your browser to sign in. If it does not open, visit:")
                }
                Flow::LoopbackUrl => println!("Open this URL in a browser to sign in:"),
                Flow::Code(reason) => {
                    match reason {
                        CodeReason::Requested => {}
                        CodeReason::RemoteSession => println!(
                            "This looks like a remote shell, so Aster will ask for a code instead of a redirect."
                        ),
                        CodeReason::BrowserFailed => println!(
                            "Could not open a browser, so Aster will ask for a code instead of a redirect."
                        ),
                    }
                    println!("Open this URL in a browser on any computer and sign in:");
                }
            }
            println!("{}", announcement.url);
            println!(
                "Confirm only if the page shows this code: {}",
                announcement.user_code
            );
            if copy {
                println!("Request id: {}", announcement.request_id);
                println!("{}", aster::cache::PASTE_WARNING);
                print!("Paste the code the page shows, then press Enter: ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            } else {
                println!("Waiting for the browser (Ctrl-C to cancel)...");
            }
        },
        &mut std::io::stdin().lock(),
    )?;
    if !quiet {
        let who = signed_in
            .email
            .as_deref()
            .map(|email| format!(" as {email}"))
            .unwrap_or_default();
        let until = chrono::DateTime::from_timestamp(signed_in.expires_at as i64, 0)
            .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "it expires".to_string());
        println!(
            "Signed in{who}. Token stored at {} until {until}.",
            signed_in.path.display()
        );
    }
    Ok(())
}

/// `aster srv logout`.
fn srv_logout() -> Result<()> {
    let server = srv_login_url(None, "logout")?;
    let signed_out = aster::cache::logout(&server)?;
    if let Some(path) = &signed_out.deleted {
        println!("Deleted {}.", path.display());
    }
    match signed_out.revoked {
        Ok(()) => {
            println!("Signed out: the server refuses every token issued to you before now.");
            Ok(())
        }
        Err(reason) if signed_out.deleted.is_none() => {
            println!("Nothing to do: {reason}.");
            Ok(())
        }
        Err(reason) => anyhow::bail!(
            "the local token is deleted, but the server was not told: {reason}; \
             run aster srv login and aster srv logout again to revoke it"
        ),
    }
}

/// `aster srv whoami`.
fn srv_whoami(url: Option<&str>) -> Result<()> {
    let server = srv_login_url(url, "whoami")?;
    let stored = aster::cache::stored_token(&server, std::time::SystemTime::now())
        .ok_or_else(|| anyhow::anyhow!("not signed in to {server}; run aster srv login"))?;
    let until = chrono::DateTime::from_timestamp(stored.expires_at as i64, 0)
        .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "it expires".to_string());
    let who = stored.email.as_deref().unwrap_or("an unknown account");
    println!("Signed in to {} as {who} until {until}.", stored.server);
    Ok(())
}

/// `aster srv ...`.
fn handle_srv(command: &SrvCommands, output_mode: OutputMode) -> Result<()> {
    use aster::test_results::read::Dataset;
    let (path, target, renamed) = match command {
        SrvCommands::Login {
            no_browser,
            loopback,
        } => return srv_login(*no_browser, *loopback, output_mode),
        SrvCommands::Logout => return srv_logout(),
        SrvCommands::Whoami { url } => return srv_whoami(url.as_deref()),
        SrvCommands::Get { path, target } => {
            if !path.starts_with('/') {
                anyhow::bail!("the path must start with /, for example /v1/cache/summary");
            }
            (path.clone(), target, None)
        }
        SrvCommands::Cache { target } => {
            (Dataset::CacheSummary.path(), target, Some("cache stats"))
        }
        SrvCommands::Flaky { target } => (Dataset::Flaky.path(), target, Some("results flaky")),
        SrvCommands::Broken { target } => (Dataset::Broken.path(), target, Some("results broken")),
        SrvCommands::Quarantine { target } => (
            Dataset::Quarantine.path(),
            target,
            Some("results quarantine"),
        ),
        SrvCommands::Clusters { target } => {
            (Dataset::Clusters.path(), target, Some("results clusters"))
        }
        SrvCommands::Test { id, target } => (
            Dataset::Test(id.clone()).path(),
            target,
            Some("results test"),
        ),
    };
    if let Some(renamed) = renamed {
        eprintln!("[aster] this command is now `aster {renamed}`");
    }
    print_srv_dataset(&path, target)
}

/// Print one aster-srv `/v1` GET as JSON.
fn print_srv_dataset(path: &str, target: &SrvTarget) -> Result<()> {
    use aster::test_results::read;
    let url = srv_url(target.url.as_deref())
        .ok_or_else(|| anyhow::anyhow!("give --url or set ASTER_SRV_URL to your aster-srv URL"))?;
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let root = find_workspace_root(&cwd).unwrap_or(cwd);
    let env_var = |name: &str| env::var(name).ok();
    let repo = read::default_repo(target.repo.as_deref(), &root, &env_var);
    let body = read::get(
        &url,
        &read::with_repo(path, repo.as_deref()),
        &env_var,
        std::time::SystemTime::now(),
    )?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// The quarantine for a run, loaded once per invocation (a run over
/// several lanes executes several times). Only a run that captures test
/// results can use one.
fn quarantine_for(
    workspace_root: &Path,
    capturing: bool,
) -> Result<Option<aster::test_results::quarantine::Quarantine>> {
    static LOADED: std::sync::OnceLock<
        std::result::Result<Option<aster::test_results::quarantine::Quarantine>, String>,
    > = std::sync::OnceLock::new();
    if !capturing {
        return Ok(None);
    }
    LOADED
        .get_or_init(|| {
            aster::test_results::quarantine::load(workspace_root, &|name| env::var(name).ok())
                .map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// The server URL from `--url`, else `ASTER_SRV_URL`.
fn srv_url(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| env::var("ASTER_SRV_URL").ok())
        .filter(|url| !url.is_empty())
}

/// In GitHub Actions, add the run's failures to the job's step summary and
/// annotate them, unless `ASTER_GITHUB_SUMMARY=0`. Annotations go to stderr
/// like the retry warnings, so `--json` output stays clean; a run that retried
/// has already warned about its flaky tests. Never fails the build.
fn github_test_summary(path: &Path) {
    let set = |name: &str| env::var(name).ok().filter(|v| !v.is_empty());
    if env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
        || set("GITHUB_STEP_SUMMARY").is_none()
        || env::var("ASTER_GITHUB_SUMMARY").as_deref() == Ok("0")
    {
        return;
    }
    let result = aster::test_results::summary::run(
        &aster::test_results::summary::Options {
            path,
            url: srv_url(None),
            github: true,
            flaky_warnings: false,
        },
        &|name| env::var(name).ok(),
        &mut io::stderr().lock(),
    );
    if let Err(error) = result {
        eprintln!("[aster] Warning: test results summary not written: {error:#}");
    }
}

/// Upload a results file, reporting the outcome on stderr. Returns whether
/// it was stored.
fn upload_test_results(url: &str, path: &Path, output_mode: OutputMode) -> bool {
    use aster::test_results::upload::{upload, Credentials};
    let Some(credentials) = Credentials::from_env(&|name| env::var(name).ok()) else {
        eprintln!(
            "[aster] Warning: test results not uploaded: no credentials. In GitHub Actions \
             grant `id-token: write`; elsewhere set ASTER_SRV_TOKEN."
        );
        return false;
    };
    let proxy_token = env::var("ASTER_SRV_PROXY_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    match upload(url, path, &credentials, proxy_token.as_deref()) {
        Ok(receipt) => {
            if output_mode != OutputMode::Quiet {
                eprintln!(
                    "[aster] Test results uploaded: run {}{}",
                    receipt.run_id,
                    if receipt.duplicate {
                        " (already stored)".to_string()
                    } else {
                        format!(" ({} tests)", receipt.tests)
                    }
                );
            }
            true
        }
        Err(error) => {
            eprintln!("[aster] Warning: test results not uploaded: {error:#}");
            false
        }
    }
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

fn print_cache_status(
    status: &aster::cache::CacheStatus,
    target: Option<&str>,
    output_mode: OutputMode,
) -> Result<()> {
    if output_mode == OutputMode::Json {
        if target.is_some_and(|addr| addr.contains(':')) && status.targets.is_empty() {
            println!("null");
            return Ok(());
        }
        output_json(status)?;
        return Ok(());
    }

    if let Some(addr) = target.filter(|addr| addr.contains(':')) {
        if status.targets.is_empty() {
            println!("{addr}: not cached");
            return Ok(());
        }
    }

    println!("Cache dir: {}", status.cache_dir);
    if status.targets.is_empty() {
        println!("No cached targets");
        return Ok(());
    }
    println!("Cached targets ({}):", status.targets.len());
    println!(
        "  {:<24} {:<12} {:>8} {:>12} {:>6}  CREATED",
        "ADDRESS", "KEY", "OUTPUTS", "BYTES", "EXIT"
    );
    for row in &status.targets {
        let key = &row.key[..CACHE_HASH_DISPLAY_LENGTH.min(row.key.len())];
        let exit = row
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<24} {:<12} {:>8} {:>12} {:>6}  {}",
            row.address, key, row.outputs, row.output_bytes, exit, row.created_at
        );
    }
    Ok(())
}

/// Print every component of `addr`'s cache key, using the same hasher the
/// executor uses. Dependency keys are recomputed from current inputs.
fn print_cache_inputs(
    addr: &str,
    workspace_root: &Path,
    projects: &[DiscoveredProject],
    registry: &aster::plugins::PluginRegistry,
    output_mode: OutputMode,
) -> Result<()> {
    use aster::cache::{CacheHasher, DependencyHash, RuntimeInputCache};

    let (project_addr, target_name) = parse_target_address(addr)
        .ok_or_else(|| anyhow::anyhow!("invalid target address '{addr}'"))?;
    let project = projects
        .iter()
        .find(|project| project_address(project) == project_addr)
        .ok_or_else(|| anyhow::anyhow!("project '{project_addr}' not found"))?;
    let target = project
        .targets
        .get(&target_name)
        .ok_or_else(|| anyhow::anyhow!("target '{target_name}' not found in {project_addr}"))?;
    let plugin = registry
        .find_by_name(&project.plugin_name)
        .ok_or_else(|| anyhow::anyhow!("plugin '{}' not found", project.plugin_name))?;

    let mut inputs = plugin.cache_inputs(&target_name);
    inputs.workspace_files =
        plugin.workspace_cache_inputs(&project.root, workspace_root, &target_name);

    // Dependency keys are recomputed from current inputs, as a fresh run
    // would, so a stale stored entry cannot change the displayed key.
    let env_snapshot: HashMap<String, String> = std::env::vars().collect();
    let runtime = RuntimeInputCache::new(workspace_root);
    let project_map: HashMap<String, &DiscoveredProject> = projects
        .iter()
        .map(|project| (project_address(project), project))
        .collect();
    let mut fresh_keys = HashMap::new();
    let deps: Vec<DependencyHash> = target
        .depends_on
        .iter()
        .map(|dep| DependencyHash {
            hash: aster::executor::fresh_cache_key(
                dep,
                &project_map,
                workspace_root,
                registry,
                &env_snapshot,
                &runtime,
                &mut fresh_keys,
            ),
            address: dep.clone(),
        })
        .collect();

    let excluded = nested_project_roots(project, projects);
    let hasher = CacheHasher::new(&project.root, workspace_root).with_excluded_dirs(excluded);
    let (material, key) = hasher.explain(
        &inputs,
        target.cache.as_ref(),
        &target.command,
        &deps,
        &env_snapshot,
        &runtime,
    );

    if output_mode == OutputMode::Json {
        let value = serde_json::to_value(&material)?;
        let key_value = key.as_ref().cloned().unwrap_or_default();
        let mut ordered = serde_json::Map::new();
        ordered.insert("key".to_string(), serde_json::Value::String(key_value));
        if let Some(object) = value.as_object() {
            for (field, field_value) in object {
                ordered.insert(field.clone(), field_value.clone());
            }
        }
        ordered.insert(
            "outputs".to_string(),
            serde_json::Value::Array(
                effective_outputs(target)
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        output_json(&ordered)?;
    } else {
        println!("salt: {}", material.salt.replace('\n', " "));
        println!("platform: {}", material.platform);
        if material.files.is_empty() {
            println!("files: (none)");
        } else {
            println!("files:");
            for file in &material.files {
                println!("  {} {}", file.digest, file.path);
            }
        }
        println!("command: {}", material.command);
        if material.deps.is_empty() {
            println!("deps: (none)");
        } else {
            println!("deps:");
            for dep in &material.deps {
                match &dep.hash {
                    Some(hash) => println!("  {} {hash}", dep.address),
                    None => println!("  {} (no key)", dep.address),
                }
            }
        }
        if material.env.is_empty() {
            println!("env: (none)");
        } else {
            println!("env:");
            for var in &material.env {
                match &var.value_hash {
                    Some(hash) => println!("  {} set {hash}", var.name),
                    None => println!("  {} unset", var.name),
                }
            }
        }
        if !material.path_env.is_empty() {
            println!("path-env:");
            for var in &material.path_env {
                if let Some(hash) = &var.value_hash {
                    println!("  {} set {hash}", var.name);
                }
            }
        }
        if material.runtime.is_empty() {
            println!("runtime: (none)");
        } else {
            println!("runtime:");
            for input in &material.runtime {
                match &input.output_hash {
                    Some(hash) => println!("  {} {hash}", input.cmd),
                    None => println!(
                        "  {} failed: {}",
                        input.cmd,
                        input.error.as_deref().unwrap_or("unknown error")
                    ),
                }
            }
        }
        let outputs = effective_outputs(target);
        if outputs.is_empty() {
            println!("outputs: (none)");
        } else {
            println!("outputs:");
            for output in outputs {
                println!("  {output}");
            }
        }
        match &key {
            Ok(key) => println!("key: {key}"),
            Err(_) => println!("key: (unavailable)"),
        }
    }

    key.map(|_| ())
}

/// Output globs the artifact cache stores for this resolved target.
///
/// Discovery already replaced an omitted `outputs` field with the plugin
/// default, and left a user-set list (including empty) in place.
fn effective_outputs(target: &aster::plugins::Target) -> Vec<String> {
    target
        .cache
        .as_ref()
        .and_then(|cache| cache.outputs.clone())
        .unwrap_or_default()
}

fn project_address(project: &DiscoveredProject) -> String {
    format!("//{}", project.relative_path.display())
}

fn nested_project_roots(
    project: &DiscoveredProject,
    projects: &[DiscoveredProject],
) -> Vec<PathBuf> {
    let parent = project.root.as_path();
    let mut roots: Vec<PathBuf> = projects
        .iter()
        .filter(|other| other.root != parent && other.root.starts_with(parent))
        .map(|other| other.root.clone())
        .collect();
    roots.sort();
    roots.dedup();
    roots
}
