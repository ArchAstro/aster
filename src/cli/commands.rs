//! CLI command definitions using clap derive
//!
//! Defines the main CLI structure and available subcommands.

use clap::{Parser, Subcommand};

/// Build orchestration for polyglot monorepos
#[derive(Parser)]
#[command(name = "aster")]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

/// Available commands
#[derive(Subcommand)]
pub enum Commands {
    /// List all discovered projects in the workspace
    List,

    /// Show the dependency graph
    Graph {
        /// Specific project to show dependencies for (//path/to/project)
        project: Option<String>,
    },
}
