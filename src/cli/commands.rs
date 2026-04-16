//! CLI command definitions using clap derive
//!
//! Defines the main CLI structure and available subcommands.

use clap::{Parser, Subcommand};

use super::output::OutputMode;

/// Build orchestration for polyglot monorepos
#[derive(Parser)]
#[command(name = "aster")]
#[command(version, about, long_about = None)]
#[command(after_help = r#"RUNNING TARGETS:
  Run any target (test, build, lint, etc.) on your projects:

    aster test --all              Run tests on all projects
    aster test //services/api     Run tests on specific project (+ dependencies)
    aster build //a //b           Run build on multiple projects
    aster lint .                  Run lint on project in current directory
    aster test //src/ts/...       Run tests on all projects under src/ts/

  Project selection patterns:
    //path/to/project     Exact project match
    //path/prefix/...     All projects under path prefix
    //...                 All projects (same as --all)
    ./...                 All projects under current directory
    -//path/prefix/...    Exclude projects matching pattern

  Flags for target execution:
    --all                 Run on all projects in the workspace
    --no-deps             Skip running dependencies first
    --dependents          Also run projects that depend on selected ones
    --warnings-as-errors  Treat warnings as errors (for supported targets)
    --lang <langs>        Filter by language (e.g., --lang nodejs,python)

EXAMPLES:
    aster test --all                     # Test everything
    aster test //...                     # Same as above
    aster test //libs/core               # Test core and its dependencies
    aster test //libs/core --no-deps     # Test only core, skip dependencies
    aster test //src/ts/...              # Test all projects under src/ts/
    aster test ./...                     # Test all projects under cwd
    aster test //... -//vendor/...       # Test all except vendor projects
    aster build //app --dependents       # Build app and everything that uses it
    aster affected test --base=main      # Test projects changed since main

HETEROGENEOUS RUNS:
    aster run //a:test //b:build //c:lint   # Run different targets on different projects
    aster run //a:test //b:test --no-deps   # Run without unlisted dependencies"#)]
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

    /// Show full output for failed targets instead of truncated logs
    ///
    /// By default, only the last 15 lines of output are shown for failures.
    /// Use this flag to see the complete output, useful for CI environments.
    #[arg(long, global = true)]
    pub full_logs: bool,

    /// Disable caching, force re-run of all targets
    #[arg(long, global = true)]
    pub no_cache: bool,
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

    /// Check if full logs should be shown for failures
    pub fn full_logs(&self) -> bool {
        self.full_logs
    }
}

/// Available commands
#[derive(Subcommand)]
pub enum Commands {
    /// List all discovered projects in the workspace
    List {
        /// Directory to scope listing to (e.g., "services/" or ".")
        path: Option<String>,

        /// Filter by language (e.g., nodejs, python, rust, go, elixir)
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,
    },

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

        /// Show what would run without executing
        #[arg(long)]
        dry_run: bool,

        /// Pass affected files to targets that support it
        ///
        /// For targets with FilesList capability, passes only the changed files
        /// instead of running the full target. Useful for running tests only
        /// on files that changed.
        #[arg(long)]
        only_affected_files: bool,

        /// Treat warnings as errors for targets that support it
        ///
        /// For targets with WarningsAsErrors capability, modifies the command
        /// to fail on warnings. Useful for CI to catch potential issues.
        #[arg(long)]
        warnings_as_errors: bool,

        /// Filter by language (e.g., nodejs, python, rust, go, elixir)
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,
    },

    /// View logs from the last run
    Logs {
        /// Specific target to view (e.g., //services/api:test)
        target: Option<String>,
    },

    /// Run a heterogeneous set of targets
    ///
    /// Execute multiple project:target pairs in dependency order.
    /// Useful when you need to run different targets on different projects.
    Run {
        /// Targets to run (//project:target format)
        #[arg(required = true)]
        targets: Vec<String>,

        /// Skip dependencies not explicitly listed
        #[arg(long)]
        no_deps: bool,

        /// Filter by language (e.g., nodejs, python, rust, go, elixir)
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,
    },

    /// Project-level commands
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },

    /// Manage the build cache
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },

    /// Watch targets and rerun them when their declared inputs change
    ///
    /// Pass one or more target addresses (//project:target) or bare projects
    /// (//project uses the --target default). Watching a target implicitly
    /// watches its transitive dependency closure.
    ///
    /// Targets with `stream = true` (dev servers, etc.) are kept running and
    /// restarted when their inputs or any dependency's inputs change.
    Watch {
        /// Targets to watch (//project:target or //project)
        #[arg(required = true)]
        targets: Vec<String>,

        /// Default target name for bare project addresses
        #[arg(long, default_value = "build")]
        target: String,

        /// Debounce window for coalescing rapid file events (e.g. "300ms", "1s")
        #[arg(long)]
        debounce: Option<String>,

        /// Skip the one-shot initial run on startup
        #[arg(long)]
        no_initial: bool,

        /// Filter by language (e.g., --lang nodejs,python)
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,
    },

    /// Run a single target on projects (catch-all for targets like test, build, lint)
    #[command(external_subcommand)]
    Target(Vec<String>),
}

/// Cache subcommands
#[derive(Subcommand)]
pub enum CacheCommands {
    /// Clear cached results
    Clear {
        /// Specific target to clear (e.g., //services/api:test)
        /// If not specified, clears all cache
        target: Option<String>,
    },

    /// Show cache status
    Status {
        /// Specific target to show (e.g., //services/api:test)
        target: Option<String>,
    },
}

/// Project-level subcommands
#[derive(Subcommand)]
pub enum ProjectCommands {
    /// Initialize an aster.toml config file in the current project
    ///
    /// Creates a project configuration file with helpful examples based on
    /// the detected language. If no language is detected, provides a generic
    /// template for custom targets.
    Init {
        /// Path to project directory (default: current directory)
        #[arg(default_value = ".")]
        path: String,

        /// Overwrite existing aster.toml
        #[arg(long)]
        force: bool,
    },
}
