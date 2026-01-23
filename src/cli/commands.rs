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

    /// Show the target dependency graph
    Graph {
        /// Specific target to show dependencies for (//path/to/project:target)
        target: Option<String>,
    },

    /// Show the dependency path between two targets
    Why {
        /// Source target (//path/to/project:target)
        from: String,
        /// Destination target (//path/to/project:target)
        to: String,
    },

    /// Initialize an aster workspace
    Init,

    /// Run a target on projects affected by git changes
    Affected {
        /// Target to run (test, build, lint, etc.)
        target: String,

        /// Base ref for comparison (default: main)
        #[arg(long, default_value = "main")]
        base: String,

        /// Head ref for comparison (default: HEAD + uncommitted)
        #[arg(long)]
        head: Option<String>,

        /// Also run dependents of affected projects
        #[arg(long)]
        dependents: bool,
    },

    /// Run a target on projects (catch-all for targets like test, build, lint)
    #[command(external_subcommand)]
    Run(Vec<String>),
}
