//! Aster CLI entry point
//!
//! Build orchestration for polyglot monorepos.

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::process::ExitCode;

use aster::cli::{Cli, Commands};
use aster::config::find_workspace_root;
use aster::discovery::discover_projects;
use aster::graph::{build_graph, find_cycle};
use aster::plugins::{NodeJsPlugin, PluginRegistry};

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

    // Find workspace root
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let workspace_root = find_workspace_root(&cwd)
        .context("Not in an aster workspace (no aster.toml or .git found)")?;

    if cli.verbose {
        eprintln!("Workspace root: {}", workspace_root.display());
    }

    // Set up plugin registry
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(NodeJsPlugin));

    // Discover projects
    let projects = discover_projects(&workspace_root, &registry)
        .context("Failed to discover projects")?;

    if cli.verbose {
        eprintln!("Discovered {} projects", projects.len());
    }

    match cli.command {
        Commands::List => {
            for project in &projects {
                println!("//{}", project.relative_path.display());
            }
        }
        Commands::Graph { project } => {
            // Build the graph
            let graph = build_graph(&projects)?;

            // Check for cycles
            if let Some(cycle) = find_cycle(&graph) {
                eprintln!("error: {cycle}");
                return Err(anyhow::anyhow!("Dependency cycle detected"));
            }

            // Display graph
            if let Some(addr) = project {
                // Show deps for specific project
                if let Some(node) = graph.get(&addr) {
                    println!("{}", node.address);
                    for dep in graph.dependencies(&addr) {
                        println!("  -> {}", dep.address);
                    }
                } else {
                    return Err(anyhow::anyhow!("Project not found: {}", addr));
                }
            } else {
                // Show full graph as tree
                for node in graph.projects() {
                    println!("{}", node.address);
                    for dep in graph.dependencies(&node.address) {
                        println!("  -> {}", dep.address);
                    }
                }
            }
        }
    }

    Ok(())
}
