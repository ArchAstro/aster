//! CLI command definitions using clap derive
//!
//! Defines the main CLI structure and available subcommands.

use clap::{Parser, Subcommand};

use super::output::OutputMode;

/// Build orchestration for polyglot monorepos
#[derive(Parser)]
#[command(name = "aster")]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable verbose output
    #[arg(short, long, global = true, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Suppress per-project output, show only summary
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Output in JSON format for machine consumption
    #[arg(long, global = true)]
    pub json: bool,
}

impl Cli {
    /// Determine the output mode based on flags
    pub fn output_mode(&self) -> OutputMode {
        if self.json {
            OutputMode::Json
        } else if self.verbose {
            OutputMode::Verbose
        } else if self.quiet {
            OutputMode::Quiet
        } else {
            OutputMode::Normal
        }
    }
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

    /// View logs from the last run
    Logs {
        /// Specific target to view (e.g., //services/api:test)
        target: Option<String>,
    },

    /// Run a target on projects (catch-all for targets like test, build, lint)
    #[command(external_subcommand)]
    Run(Vec<String>),
}
