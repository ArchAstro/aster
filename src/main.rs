//! Aster CLI entry point
//!
//! Build orchestration for polyglot monorepos.

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::fs;
use std::process::ExitCode;

use aster::cli::{expand_selection, parse_run_args, select_projects, Cli, Commands};
use aster::config::find_workspace_root;
use aster::discovery::discover_projects;
use aster::executor::Executor;
use aster::git::{affected_with_dependents, files_to_projects, AffectedDetector};
use aster::graph::{build_graph, build_target_graph, find_cycle, format_path};
use aster::plugins::{ElixirPlugin, NodeJsPlugin, PluginRegistry, PythonPlugin};

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

    let cwd = env::current_dir().context("Failed to get current directory")?;

    // Handle init command specially - it works even without an existing workspace
    if matches!(cli.command, Commands::Init) {
        return handle_init(&cwd, cli.verbose);
    }

    // For all other commands, require a workspace
    let workspace_root = find_workspace_root(&cwd)
        .context("Not in an aster workspace (no aster.toml or .git found). Run 'aster init' to create one.")?;

    if cli.verbose {
        eprintln!("Workspace root: {}", workspace_root.display());
    }

    // Set up plugin registry with all language plugins
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(NodeJsPlugin));
    registry.register(Box::new(ElixirPlugin));
    registry.register(Box::new(PythonPlugin));

    // Discover projects
    let projects = discover_projects(&workspace_root, &registry)
        .context("Failed to discover projects")?;

    if cli.verbose {
        eprintln!("Discovered {} projects", projects.len());
    }

    match cli.command {
        Commands::Init => unreachable!("Init handled above"),
        Commands::List => {
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
        Commands::Graph { target } => {
            // Build the target graph
            let graph = build_target_graph(&projects);

            // Check for cycles
            if let Some(cycle) = graph.find_cycle() {
                eprintln!("error: {cycle}");
                return Err(anyhow::anyhow!("Dependency cycle detected"));
            }

            // Display graph
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
            match graph.find_path(&from, &to) {
                Some(path) => {
                    println!("{}", format_path(&path));
                }
                None => {
                    println!("No dependency path found between {} and {}", from, to);
                }
            }
        }
        Commands::Affected {
            target,
            base,
            head,
            dependents,
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

            if cli.verbose {
                eprintln!("Found {} changed files", changed_files.len());
                for file in &changed_files {
                    eprintln!("  - {}", file.display());
                }
            }

            // Map files to projects
            let directly_affected = files_to_projects(&changed_files, &projects);

            if cli.verbose {
                eprintln!("Directly affected: {:?}", directly_affected);
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
                println!("No projects affected");
                return Ok(());
            }

            // Sort by dependency order
            let ordered = graph.topological_order_subset(&affected_projects);

            if cli.verbose {
                eprintln!(
                    "Running '{}' on {} affected projects",
                    target,
                    ordered.len()
                );
            }

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

            // Execute target on projects
            let executor = Executor::new(&workspace_root);
            let results = executor.execute(&target, &ordered, &graph);

            // Print summary
            let skipped = results.iter().filter(|r| r.skipped).count();
            let passed = results.iter().filter(|r| r.success && !r.skipped).count();
            let failed = results.iter().filter(|r| !r.success).count();

            println!("\n=== Summary ===");
            if skipped > 0 {
                println!(
                    "Ran '{}' on {} affected projects: {} passed, {} failed, {} skipped (no target)",
                    target,
                    results.len(),
                    passed,
                    failed,
                    skipped
                );
            } else {
                println!(
                    "Ran '{}' on {} affected projects: {} passed, {} failed",
                    target,
                    results.len(),
                    passed,
                    failed
                );
            }

            if failed > 0 {
                println!("\nFailed projects:");
                for result in results.iter().filter(|r| !r.success) {
                    println!("  - {}", result.address);
                }
                return Err(anyhow::anyhow!("{} project(s) failed", failed));
            }
        }
        Commands::Run(args) => {
            // Parse external subcommand args
            let run_args = parse_run_args(args);

            if run_args.target.is_empty() {
                return Err(anyhow::anyhow!("No target specified. Usage: aster <target> [projects...] [--all] [--no-deps] [--dependents]"));
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

            if cli.verbose {
                eprintln!(
                    "Running '{}' on {} projects",
                    run_args.target,
                    ordered.len()
                );
            }

            // Execute target on projects
            let executor = Executor::new(&workspace_root);
            let results = executor.execute(&run_args.target, &ordered, &graph);

            // Print summary
            let skipped = results.iter().filter(|r| r.skipped).count();
            let passed = results.iter().filter(|r| r.success && !r.skipped).count();
            let failed = results.iter().filter(|r| !r.success).count();

            println!("\n=== Summary ===");
            if skipped > 0 {
                println!(
                    "Ran '{}' on {} projects: {} passed, {} failed, {} skipped (no target)",
                    run_args.target,
                    results.len(),
                    passed,
                    failed,
                    skipped
                );
            } else {
                println!(
                    "Ran '{}' on {} projects: {} passed, {} failed",
                    run_args.target,
                    results.len(),
                    passed,
                    failed
                );
            }

            if failed > 0 {
                println!("\nFailed projects:");
                for result in results.iter().filter(|r| !r.success) {
                    println!("  - {}", result.address);
                }
                return Err(anyhow::anyhow!("{} project(s) failed", failed));
            }
        }
    }

    Ok(())
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
