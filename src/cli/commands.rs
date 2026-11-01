//! CLI command definitions using clap derive
//!
//! Defines the main CLI structure and available subcommands.

use clap::{CommandFactory, Parser, Subcommand};

use super::output::OutputMode;

/// Build orchestration for polyglot monorepos
#[derive(Parser)]
#[command(name = "aster")]
#[command(version, about, long_about = None)]
#[command(arg_required_else_help = true)]
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
    --lang <langs>        Filter by language (e.g., --lang nodejs,ruby)
    --stream              Force streaming output (--no-stream disables it)

  Use `aster target <name>` when a target name conflicts with a built-in command.

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
    aster target dev //services/api      # Run a target named "dev"

HETEROGENEOUS RUNS:
    aster run //a:test //b:build //c:lint   # Run different targets on different projects
    aster run //a:test //b:test --no-deps   # Run without unlisted dependencies"#)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Print a Markdown guide for using Aster, including examples for LLMs
    #[arg(long, global = true, exclusive = true)]
    pub skills: bool,

    /// Shared host capacity for target commands (also ASTER_JOBS).
    /// An active invocation freezes this value; conflicting requests fail.
    #[arg(long, global = true, value_name = "N")]
    pub jobs: Option<std::num::NonZeroUsize>,

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

    /// Do not print a cache hit's stored log.
    ///
    /// The hit is still reported (`cached: true`, stored exit code and
    /// duration). Only the replayed log is suppressed.
    #[arg(long, global = true)]
    pub no_replay: bool,

    /// Directory for the artifact cache (`cas/`, `ac/`, `index/`).
    ///
    /// A legacy `cache.json` in that directory is obsolete and unused.
    /// Overrides `ASTER_CACHE_DIR`. A relative path is resolved against the
    /// current working directory. The default is `<workspace>/.aster/cache`.
    #[arg(long, global = true, value_name = "DIR")]
    pub cache_dir: Option<std::path::PathBuf>,

    /// Record every test the run executes and write the results as NDJSON
    ///
    /// Aster reads each test runner's own report: ExUnit (mix test), go
    /// test, pytest, Vitest, Jest, bun test, node --test, cargo test and
    /// nextest, Gradle and Maven. Give a path with `--test-results=PATH`; without one the file
    /// goes under $RUNNER_TEMP (or the system temp directory) and its path
    /// is printed. Setting ASTER_TEST_RESULTS to a path, or to 1, does the
    /// same.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        num_args = 0..=1,
        require_equals = true
    )]
    pub test_results: Option<Option<std::path::PathBuf>>,

    /// Rerun only the failing tests of a failed test target, up to N times
    ///
    /// When a target fails and its captured results name the failing tests,
    /// Aster reruns just those tests with the runner's own selection flags
    /// (go test -run, pytest node ids, vitest/jest -t, ...), recording every
    /// attempt. A target whose failing tests all pass on a retry reports
    /// PASS marked flaky, and is not cached. Runners that cannot select
    /// single tests (Gradle, Maven) and failures that are not tests (crashes,
    /// compile errors, signals) are not retried. A target's `retry_setup`
    /// command runs before each retry. Bare `--retry-failed` means 1;
    /// ASTER_RETRY_FAILED=N does the same. Results are captured for the
    /// retries even without --test-results, which alone decides whether a
    /// results file is written.
    #[arg(
        long,
        global = true,
        value_name = "N",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "1"
    )]
    pub retry_failed: Option<u32>,
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

    /// Where to write test results, from `--test-results` or else
    /// `ASTER_TEST_RESULTS`. `None` turns recording off; `Some(None)` means
    /// the default location.
    pub fn test_results(&self) -> Option<Option<std::path::PathBuf>> {
        if self.test_results.is_some() {
            return self.test_results.clone();
        }
        test_results_from_env(std::env::var("ASTER_TEST_RESULTS").ok().as_deref())
    }

    /// How many times to rerun a failed target's failing tests, from
    /// `--retry-failed` or else `ASTER_RETRY_FAILED`. 0 turns retries off.
    pub fn retry_failed(&self) -> u32 {
        self.retry_failed
            .unwrap_or_else(|| retry_from_env(std::env::var("ASTER_RETRY_FAILED").ok().as_deref()))
    }
}

fn retry_from_env(value: Option<&str>) -> u32 {
    match value.map(str::trim) {
        Some("true" | "on") => 1,
        Some(count) => count.parse().unwrap_or(0),
        None => 0,
    }
}

fn test_results_from_env(value: Option<&str>) -> Option<Option<std::path::PathBuf>> {
    match value.map(str::trim) {
        None | Some("" | "0" | "false" | "off") => None,
        Some("1" | "true" | "on") => Some(None),
        Some(path) => Some(Some(std::path::PathBuf::from(path))),
    }
}

/// Usage for external target commands (`aster test`, `aster build`, and any project target).
///
/// Printed for `aster <target> --help`/`-h` and for `aster help` of a conventional
/// target name, including outside a workspace. Every flag `parse_run_args` accepts
/// appears here.
pub const TARGET_COMMAND_USAGE: &str = r#"Run a project target (test, build, lint, and any target a project defines).

Selectors and flags are the same for every target. This does not require a workspace.
Built-in commands are listed by `aster help` and `aster --help`.
Use `aster target <name>` when a target name conflicts with a built-in command.

Selectors:
  //path/to/project     Exact project match
  //path/prefix/...     All projects under a path prefix
  //...                 All projects (same as --all)
  ./...                 All projects under the current directory
  .                     Project in the current directory
  -//path/prefix/...    Exclude projects matching a pattern

Flags:
  --all                 Run on all projects in the workspace
  --no-deps             Skip running dependencies first
  --dependents          Also run projects that depend on the selection
  --lang <langs>        Filter by language (repeatable; comma-separated or --lang=langs)
  --warnings-as-errors  Treat warnings as errors for supported targets
  --stream              Force streaming output
  --no-stream           Force non-streaming output
  --jobs <N>            Shared host capacity (also --jobs=N)
  --no-cache            Disable caching
  --verbose, -v         Enable verbose output
  --quiet, -q           Suppress per-project output
  --json                Output JSON
  --full-logs           Show full output for failed targets
  --help, -h            Print this usage and exit without running the target
"#;

/// Target names `aster help <name>` recognizes without loading a workspace.
///
/// Any other unknown name still produces clap's unrecognized-subcommand error.
/// `aster <name> --help` prints [`TARGET_COMMAND_USAGE`] for every name, including
/// targets that are not in this list.
const CONVENTIONAL_TARGET_NAMES: &[&str] = &["build", "test", "lint", "deps", "dev", "clean"];

/// Print target-command usage. `target` personalizes the usage line when known.
pub fn print_target_command_usage(target: Option<&str>) {
    let header = match target {
        Some(target) => format!("Usage: aster {target} [selectors...] [flags]"),
        None => "Usage: aster <target> [selectors...] [flags]".to_string(),
    };
    println!("{header}\n\n{TARGET_COMMAND_USAGE}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Whether external-target args are a help request (`--help` or `-h`).
pub fn external_target_requests_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

/// First positional token in external-target args (the target name).
pub fn external_target_name(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
}

/// `aster help <name>` for a conventional target that is not a built-in command.
///
/// Clap rejects these during parse (`unrecognized subcommand`), so this must run
/// before `Cli::parse`. Built-ins and unknown names return `None` so clap can
/// show command help or its usual error.
pub fn conventional_target_help_name(args: &[String]) -> Option<&str> {
    let positionals = positional_tokens(args);
    let [command, name] = positionals.as_slice() else {
        return None;
    };
    if *command != "help" || is_builtin_subcommand(name) || !is_conventional_target(name) {
        return None;
    }
    Some(*name)
}

/// `aster target <name> … --help`/`-h` before clap parse.
///
/// The `target` subcommand is built-in, so clap may treat `--help` as that
/// subcommand's help flag and never reach target execution. This must run
/// before `Cli::parse`. Help is recognized only after both `target` and the
/// target name (`aster target build --help`). Earlier `--help`/`-h` — root
/// help, `aster target --help`, or a flag value such as `--jobs --help` —
/// returns `None` so clap handles it.
pub fn explicit_target_help_name(args: &[String]) -> Option<&str> {
    let (name, suffix) = target_name_and_suffix(args)?;
    if super::run::parse_run_args(suffix.to_vec()).help {
        Some(name)
    } else {
        None
    }
}

/// `target <name> [suffix…]` with `<name>` and the following args.
///
/// Global value flags (`--jobs`, `--cache-dir`) are skipped the same way as
/// [`positional_tokens`], so their values are not the target name and a
/// `--help` used as a value is not target help.
fn target_name_and_suffix(args: &[String]) -> Option<(&str, &[String])> {
    let mut skip_next = false;
    let mut saw_command = false;
    for (index, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return None;
        }
        if arg.starts_with('-') {
            if matches!(arg.as_str(), "--jobs" | "--cache-dir") {
                skip_next = true;
            }
            continue;
        }
        if !saw_command {
            if arg != "target" {
                return None;
            }
            saw_command = true;
            continue;
        }
        return Some((arg.as_str(), &args[index..]));
    }
    None
}

fn is_conventional_target(name: &str) -> bool {
    CONVENTIONAL_TARGET_NAMES.contains(&name)
}

fn is_builtin_subcommand(name: &str) -> bool {
    Cli::command().find_subcommand(name).is_some()
}

fn positional_tokens(args: &[String]) -> Vec<&str> {
    let mut positionals = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            if matches!(arg.as_str(), "--jobs" | "--cache-dir") {
                skip_next = true;
            }
            continue;
        }
        positionals.push(arg.as_str());
    }
    positionals
}

/// Available commands
#[derive(Subcommand)]
pub enum Commands {
    /// Run external Go tests after shell setup; --related selection is deferred until here.
    ExecGoTest {
        /// Internal related-test selection (generated by affected --related).
        #[arg(long, hide = true)]
        selection: Option<String>,
        /// Arguments passed literally to external `go test` (not a shell function).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List all discovered projects in the workspace
    List {
        /// Directory to scope listing to (e.g., "services/" or ".")
        path: Option<String>,

        /// Filter by source language (e.g., nodejs, ruby, java, kotlin)
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,
    },

    /// Show the target dependency graph
    Graph {
        /// Specific target to show dependencies for (//path/to/project:target)
        #[arg(conflicts_with = "source")]
        target: Option<String>,

        /// Show the source graph of a change instead: each changed
        /// definition and what uses it, down to the tests
        ///
        /// Covers the uncommitted changes in the working tree unless
        /// --commit names something else. Files the change does not touch
        /// are read from the working tree.
        #[arg(long)]
        source: bool,

        /// Commit or range to show, read as `git diff` reads it (HEAD~3..HEAD,
        /// origin/main...HEAD, or one ref to compare the working tree against)
        #[arg(long, requires = "source")]
        commit: Option<String>,

        /// Only consider changed files under this directory (relative to the
        /// workspace root)
        #[arg(long, requires = "source")]
        dir: Option<String>,

        /// Only consider changed files with these extensions (comma-separated,
        /// e.g. "ex,exs")
        #[arg(long, value_delimiter = ',', requires = "source")]
        ext: Vec<String>,
    },

    /// Lint the configuration and annotations that `--related` depends on
    ///
    /// Rules: unnarrowable-test-command, suspicious-path-literal,
    /// bad-consumes, orphan-test-target, stale-annotation, stale-allow-orphan,
    /// stale-allow-unnarrowable, uncaptured-test-results,
    /// unquarantinable-test-results, unretryable-test-target,
    /// stale-allow-no-test-results. Exit status 0 for
    /// no findings or only warnings, 1 for an error finding (or any finding
    /// with --strict), 2 when the configuration cannot be loaded. Use the
    /// global --json for machine-readable output.
    Check {
        /// Exit 1 on any finding, not only on errors
        #[arg(long)]
        strict: bool,

        /// Run only this rule (repeatable)
        #[arg(long, value_name = "ID")]
        rule: Vec<String>,

        /// A target CI requests, from which orphan-test-target looks for
        /// test targets that never run (repeatable; adds to
        /// `[check] entries` and the lanes' targets)
        #[arg(long, value_name = "TARGET")]
        entry: Vec<String>,
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

        /// Select a configured named subset of affected primary projects
        ///
        /// Repeat the flag with --dry-run to plan several lanes from one
        /// analysis of the change; the JSON output then has one plan per
        /// lane under `lanes`.
        #[arg(long)]
        lane: Vec<String>,

        /// Show what would run without executing
        #[arg(long)]
        dry_run: bool,

        /// Pass affected files to targets that support it
        ///
        /// Targets with the files_list capability (the requested target and
        /// same-project targets it depends on) run on the project's changed
        /// files only; Rust `cargo test` targets run the tests related to the
        /// change. Projects selected only through --dependents run in full.
        #[arg(long)]
        only_affected_files: bool,

        /// Run only the tests that reach the change, across projects
        ///
        /// Builds a source-level dependency graph (Elixir, TypeScript and
        /// JavaScript, Go, Python, Rust) and follows the changed functions, types
        /// and modules to the tests that refer to them. Test commands run
        /// narrowed to those tests; projects no change reaches are skipped.
        /// Implies --dependents, decided per symbol instead of per project.
        #[arg(long, conflicts_with = "only_affected_files")]
        related: bool,

        /// Treat warnings as errors for targets that support it
        ///
        /// For targets with WarningsAsErrors capability, modifies the command
        /// to fail on warnings. Useful for CI to catch potential issues.
        #[arg(long)]
        warnings_as_errors: bool,

        /// Filter by source language (e.g., nodejs, ruby, java, kotlin)
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,

        /// Treat every project as changed
        ///
        /// Selects every project (in each lane, when lanes are given) without
        /// reading Git changes, and runs targets in full. Use it for a
        /// scheduled or post-merge run that must cover everything with the
        /// same command line as a change-driven run.
        #[arg(long, conflicts_with_all = ["only_affected_files", "head"])]
        all: bool,

        /// Print the CI matrix: one JSON row per lane the change selects
        ///
        /// Plans every configured lane (or the ones given with --lane) and
        /// prints a JSON array with a row for each lane that has targets to
        /// run: `lane`, `name`, `target`, `toolchains`, and the lane's
        /// `matrix` fields. A CI system fans the rows out into jobs, each
        /// running `aster affected <target> --lane <lane>`. Implies --dry-run.
        #[arg(long)]
        matrix: bool,
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

        /// Filter by source language (e.g., nodejs, python, java, kotlin)
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

    /// CI helpers
    Ci {
        #[command(subcommand)]
        command: CiCommands,
    },

    /// Work with recorded test results
    Results {
        #[command(subcommand)]
        command: ResultsCommands,
    },

    /// Sign in to aster-srv, and read any of its routes
    ///
    /// `login` signs this machine in for the shared cache and the
    /// `aster results` readers. `get` prints any read-only `/v1` route as
    /// JSON. Exits non-zero with a message when you are not signed in or the
    /// server refuses.
    Srv {
        #[command(subcommand)]
        command: SrvCommands,
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

    /// Manage configured local development services
    Services {
        #[command(subcommand)]
        command: ServicesCommands,
    },

    /// Run a target whose name conflicts with a built-in command
    #[command(name = "target")]
    RunTarget {
        /// Target name followed by normal target selectors and flags
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Run a single target on projects (catch-all for targets like test, build, lint)
    #[command(external_subcommand)]
    ExternalTarget(Vec<String>),
}

/// Local development service commands
#[derive(Subcommand)]
pub enum ServicesCommands {
    /// Start configured services in a supervised dashboard
    Up {
        /// Optional group from `[dev.service_groups]`; defaults to ungrouped services
        group: Option<String>,

        /// Disable dependency-aware file watching and automatic restarts
        #[arg(long)]
        no_watch: bool,

        /// Use line-oriented logs instead of the interactive dashboard
        #[arg(long, conflicts_with = "daemon")]
        no_ui: bool,

        /// Resolve and validate the service plan without starting processes
        #[arg(long, conflicts_with = "daemon")]
        dry_run: bool,

        /// Run the service bundle headlessly under the per-user daemon
        #[arg(long)]
        daemon: bool,

        /// Start configured per-service proxies on their advertised ports
        #[arg(long)]
        proxy: bool,
    },

    /// List daemon-managed bundles in the current worktree
    List,

    /// Stop daemon-managed bundles in the current worktree
    Down {
        /// Optional exact group; omit to stop every bundle in this worktree
        group: Option<String>,
    },

    /// Manage the per-user service daemon
    Daemon {
        #[command(subcommand)]
        command: ServicesDaemonCommands,
    },

    /// Read the durable log for a configured service
    Logs {
        /// Service name from `[dev.services]`
        service: String,
    },

    /// List ports allocated to running services in this worktree
    Ports,

    /// Terminate processes listening on configured or specified ports
    KillPorts {
        /// Port numbers or configured/allocated names (defaults to all known workspace ports)
        ports: Vec<String>,

        /// Show matching processes without terminating them
        #[arg(long)]
        dry_run: bool,
    },

    /// Generate certificates or run a configured local HTTPS edge
    Tls {
        #[command(subcommand)]
        command: TlsCommands,
    },
}

/// Per-user service daemon commands.
#[derive(Subcommand)]
pub enum ServicesDaemonCommands {
    /// Stop every machine-wide bundle and the daemon
    Stop,
    /// Run the daemon server (internal use only)
    #[command(hide = true)]
    Serve,
}

/// Local TLS edge commands.
#[derive(Subcommand)]
pub enum TlsCommands {
    /// Trust mkcert's local CA and generate the configured certificate
    Setup {
        /// Name from `[dev.services.<name>]`
        edge: String,
    },
    /// Serve the configured HTTPS edge until interrupted
    Serve {
        /// Name from `[dev.services.<name>]`
        edge: String,
    },
}

/// CI subcommands
#[derive(Subcommand)]
pub enum CiCommands {
    /// Print actions/cache entries for native tool caches
    ///
    /// Resolves every selected project's native caches into concrete
    /// entries. Shared caches with the same id and paths are emitted once.
    /// With no addresses, every discovered project is included.
    CachePlan {
        /// Project addresses (`//path`, `//prefix/...`, `./...`, `-//excl`)
        ///
        /// The same selectors as `aster run`. Omit them to include every
        /// discovered project.
        projects: Vec<String>,

        /// Include every discovered project (the default when no addresses
        /// are given)
        #[arg(long)]
        all: bool,

        /// Write the combined entry to $GITHUB_OUTPUT
        ///
        /// Emits `paths` (newline-joined, heredoc syntax), `key`, and
        /// `restore-keys` so one actions/cache step can consume them.
        #[arg(long)]
        github_output: bool,
    },
}

/// Cache subcommands
#[derive(Subcommand)]
pub enum CacheCommands {
    /// Clear cached results
    ///
    /// `//proj:target` removes that address from the artifact index.
    /// `//proj` removes the project's index (`invalidate_project`).
    /// With no argument, every project index is removed. Blobs stay until
    /// `aster cache prune`.
    Clear {
        /// Specific target to clear (e.g., //services/api:test)
        /// If not specified, clears all cache
        target: Option<String>,
    },

    /// Show cache status
    ///
    /// Lists artifact-index entries: address, key, created time, output count,
    /// on-disk output bytes, and exit code. `--json` prints
    /// `{"cache_dir","targets":[...]}`. A target address filters to that
    /// address; a project address (`//proj`) filters to that project.
    Status {
        /// Specific target to show (e.g., //services/api:test)
        target: Option<String>,

        /// Print every component of the target's cache key
        ///
        /// Lists the salt, platform, each hashed file and its digest, the
        /// command, dependency hashes, tracked environment variables (names
        /// and whether set; values are SHA256-hashed), runtime inputs, and
        /// the effective output globs, then the key execution stores.
        /// Requires a target address.
        #[arg(long)]
        inputs: bool,
    },

    /// The shared cache's size, lookups and last writer, from aster-srv
    ///
    /// Prints what the dashboard's Cache page shows, as JSON. Uses the
    /// sign-in `aster srv login` stored, or CI credentials.
    Stats {
        #[command(flatten)]
        target: SrvTarget,
    },

    /// Renamed to `aster srv login`
    #[command(hide = true)]
    Login {
        #[arg(long)]
        no_browser: bool,
        #[arg(long)]
        loopback: bool,
    },

    /// Renamed to `aster srv logout`
    #[command(hide = true)]
    Logout,

    /// Evict old and least-recently-used artifact cache entries
    ///
    /// Manifests older than --max-age are removed first. Remaining manifests
    /// are removed oldest-first until the on-disk blob store is at or under
    /// --max-size. Blobs still referenced by a surviving manifest are kept.
    ///
    /// Sizes accept bytes or a KB/MB/GB suffix (powers of 1024). Ages accept
    /// s/m/h/d. `0` disables that limit. Flags override `[cache]` in the root
    /// aster.toml. Absent flags and config use 10GB and 14d.
    Prune {
        /// Maximum on-disk blob size to keep (bytes or KB/MB/GB)
        #[arg(long, value_name = "SIZE")]
        max_size: Option<String>,

        /// Maximum age of a manifest, using mtime as last access (s/m/h/d)
        #[arg(long, value_name = "AGE")]
        max_age: Option<String>,
    },
}

/// Which server and repository `aster srv` reads.
#[derive(clap::Args, Clone)]
pub struct SrvTarget {
    /// The aster-srv URL [default: $ASTER_SRV_URL]
    #[arg(long)]
    pub url: Option<String>,
    /// The repository, owner/name [default: $GITHUB_REPOSITORY, else the checkout's origin]
    #[arg(long)]
    pub repo: Option<String>,
}

/// `aster srv` subcommands
#[derive(Subcommand)]
pub enum SrvCommands {
    /// Sign in to aster-srv from this machine
    ///
    /// Opens `$ASTER_SRV_URL/cli-auth` in your browser. Sign in there with
    /// the company account the server's proxy uses and confirm only if the
    /// page shows the code this command printed. Aster receives the answer
    /// on a one-shot listener on 127.0.0.1 and stores a read-only token
    /// under your config directory ($XDG_CONFIG_HOME/aster, else
    /// ~/.config/aster), readable only by you, until it expires (hours).
    /// Later `aster run`s with `ASTER_SRV_URL` set restore from the shared
    /// cache, and `aster results flaky` and the other readers use it; without
    /// a sign-in a run prints one line and uses the local cache.
    ///
    /// On a remote shell (SSH, a devbox) the browser cannot reach that
    /// listener, so Aster prints a URL instead: open it in any browser,
    /// confirm, and paste the one-time code the page shows back into the
    /// terminal. That happens with `--no-browser`, and automatically when
    /// SSH_CONNECTION, SSH_CLIENT or SSH_TTY is set, when a Linux shell has
    /// neither DISPLAY nor WAYLAND_DISPLAY, or when the browser fails to
    /// open. `--loopback` forces the browser redirect.
    Login {
        /// Do not open a browser: print a URL and paste back the code
        #[arg(long)]
        no_browser: bool,
        /// Always use the browser redirect to 127.0.0.1 (with --no-browser:
        /// print the URL, and forward the port with `ssh -R`)
        #[arg(long)]
        loopback: bool,
    },

    /// Sign out of aster-srv
    ///
    /// Deletes the stored token and asks the server to refuse every token
    /// issued to you so far, on every machine.
    Logout,

    /// Show who this machine is signed in to aster-srv as
    ///
    /// Reads the stored sign-in for `$ASTER_SRV_URL` (or --url) and prints the
    /// email and when it expires. Exits 1 when there is no usable sign-in.
    /// Does not contact the server.
    Whoami {
        /// The aster-srv URL [default: $ASTER_SRV_URL]
        #[arg(long)]
        url: Option<String>,
    },

    /// GET any aster-srv path and print the JSON
    ///
    /// For example `aster srv get '/v1/test-results/tests?state=flaky&limit=20'`.
    /// A `/v1/` path that names no `repo` gets the default one.
    Get {
        /// The path and query, starting with /
        path: String,
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Renamed to `aster cache stats`
    #[command(hide = true)]
    Cache {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Renamed to `aster results flaky`
    #[command(hide = true)]
    Flaky {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Renamed to `aster results broken`
    #[command(hide = true)]
    Broken {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Renamed to `aster results quarantine`
    #[command(hide = true)]
    Quarantine {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Renamed to `aster results clusters`
    #[command(hide = true)]
    Clusters {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Renamed to `aster results test`
    #[command(hide = true)]
    Test {
        id: String,
        #[command(flatten)]
        target: SrvTarget,
    },
}

/// Test results subcommands
#[derive(Subcommand)]
pub enum ResultsCommands {
    /// Send a results file written by `--test-results` to aster-srv
    ///
    /// In GitHub Actions the job's OIDC token is used (the job needs
    /// `permissions: id-token: write`); elsewhere set ASTER_SRV_TOKEN. Behind
    /// an identity-aware proxy, ASTER_SRV_PROXY_TOKEN is sent as
    /// `Proxy-Authorization`. A failed upload is reported but exits 0 unless
    /// --strict, so it never fails a build.
    Upload {
        /// The NDJSON results file
        path: std::path::PathBuf,
        /// The aster-srv URL [default: $ASTER_SRV_URL]
        #[arg(long)]
        url: Option<String>,
        /// Exit non-zero when the upload fails
        #[arg(long)]
        strict: bool,
    },
    /// Report the failing tests in a results file
    ///
    /// Lists tests whose last attempt failed, and tests that failed then
    /// passed on retry, as Markdown. With a server (--url or ASTER_SRV_URL,
    /// and the credentials `upload` uses) each failing test is labelled `new`,
    /// `broken` or `known flake`; without one it is just `failed`. Looking
    /// tests up never fails the report.
    ///
    /// With --github the Markdown is appended to $GITHUB_STEP_SUMMARY and
    /// GitHub workflow commands (an error annotation per failing test, a
    /// warning per test that passed on retry) are printed. Nothing is written
    /// when there are no failures. Aster runs this itself after
    /// `--test-results` in GitHub Actions unless ASTER_GITHUB_SUMMARY=0.
    Summary {
        /// The NDJSON results file
        path: std::path::PathBuf,
        /// The aster-srv URL [default: $ASTER_SRV_URL]
        #[arg(long)]
        url: Option<String>,
        /// Write the GitHub step summary and print annotations
        #[arg(long)]
        github: bool,
    },
    /// Print the failing tests of a GitHub Actions run, from aster-srv
    ///
    /// Asks the server for every Aster run recorded under a GitHub run id
    /// (one per job or lane) and prints their failures as JSON, all pages
    /// merged: `{github_run, runs, failures, next_cursor}`. Uses the
    /// credentials and proxy token `upload` uses. Exits 1 with a message on
    /// stderr when there is no server or credentials, the server cannot be
    /// reached, or it has no runs for the id, so a script can fall back.
    Failures {
        /// The GitHub Actions run id
        #[arg(long)]
        github_run: String,
        /// Only this attempt of the run (GitHub's run_attempt)
        #[arg(long)]
        attempt: Option<u32>,
        /// The aster-srv URL [default: $ASTER_SRV_URL]
        #[arg(long)]
        url: Option<String>,
    },
    /// Write aster-durations.json from aster-srv, for balanced partitions
    ///
    /// Finds every `MIX_TEST_PARTITION=K mix test --partitions N` suite in
    /// the workspace and writes each test file's median duration, from the
    /// partitions' recent runs, to aster-durations.json at the workspace
    /// root. Commit it: each partition then splits the suite by those
    /// durations, from the checkout, so all partitions of a commit agree.
    /// Uses the credentials `upload` uses. Leaves the file unchanged and
    /// exits 1 when any suite cannot be fetched.
    Durations {
        /// The aster-srv URL [default: $ASTER_SRV_URL]
        #[arg(long)]
        url: Option<String>,
    },
    /// Flaky tests, highest score first (the Flaky tests page)
    ///
    /// This and `broken`, `quarantine`, `clusters` and `test` print what
    /// aster-srv's dashboard shows, as JSON. They use the sign-in
    /// `aster srv login` stored, or CI credentials (`ASTER_SRV_TOKEN`, GitHub
    /// OIDC) when present.
    Flaky {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// Broken tests (the Flaky tests page's broken list)
    Broken {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// The quarantine list (the Quarantine page)
    Quarantine {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// The last week's failures grouped by failure hash (the Failure clusters page)
    Clusters {
        #[command(flatten)]
        target: SrvTarget,
    },
    /// One test's state and recent results (a test's page)
    Test {
        /// The test id, as listed by `aster results flaky`
        id: String,
        #[command(flatten)]
        target: SrvTarget,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_srv_and_cache_commands_still_parse_and_are_hidden() {
        use clap::CommandFactory;
        for args in [
            &["aster", "cache", "login", "--no-browser"][..],
            &["aster", "cache", "logout"],
            &["aster", "srv", "flaky"],
            &["aster", "srv", "cache"],
            &["aster", "srv", "test", "id"],
            &["aster", "srv", "login", "--loopback"],
            &["aster", "srv", "whoami"],
            &["aster", "cache", "stats", "--repo", "o/r"],
            &["aster", "results", "quarantine"],
            &["aster", "results", "test", "id"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        let cli = Cli::command();
        let visible = |group: &str| -> Vec<String> {
            cli.find_subcommand(group)
                .unwrap()
                .get_subcommands()
                .filter(|sub| !sub.is_hide_set())
                .map(|sub| sub.get_name().to_string())
                .collect()
        };
        assert_eq!(visible("srv"), ["login", "logout", "whoami", "get"]);
        assert_eq!(visible("cache"), ["clear", "status", "stats", "prune"]);
        assert!(visible("results").contains(&"flaky".to_string()));
    }

    #[test]
    fn test_results_flag_takes_an_optional_path() {
        let cli = Cli::try_parse_from(["aster", "affected", "test", "--test-results"]).unwrap();
        assert_eq!(cli.test_results, Some(None));

        let cli = Cli::try_parse_from(["aster", "--test-results=out/r.ndjson", "list"]).unwrap();
        assert_eq!(
            cli.test_results,
            Some(Some(std::path::PathBuf::from("out/r.ndjson")))
        );

        // A following word is never taken as the path.
        let cli = Cli::try_parse_from(["aster", "--test-results", "list"]).unwrap();
        assert_eq!(cli.test_results, Some(None));
        assert!(matches!(cli.command, Some(Commands::List { .. })));
    }

    #[test]
    fn retry_failed_flag_defaults_to_one_when_bare() {
        let cli = Cli::try_parse_from(["aster", "--retry-failed", "list"]).unwrap();
        assert_eq!(cli.retry_failed, Some(1));
        let cli = Cli::try_parse_from(["aster", "--retry-failed=3", "list"]).unwrap();
        assert_eq!(cli.retry_failed, Some(3));
        let cli = Cli::try_parse_from(["aster", "list"]).unwrap();
        assert_eq!(cli.retry_failed, None);
        assert!(Cli::try_parse_from(["aster", "--retry-failed=x", "list"]).is_err());
    }

    #[test]
    fn retry_failed_env_value() {
        assert_eq!(retry_from_env(None), 0);
        assert_eq!(retry_from_env(Some("")), 0);
        assert_eq!(retry_from_env(Some("0")), 0);
        assert_eq!(retry_from_env(Some("2")), 2);
        assert_eq!(retry_from_env(Some(" 3 ")), 3);
        assert_eq!(retry_from_env(Some("true")), 1);
        assert_eq!(retry_from_env(Some("nonsense")), 0);
    }

    #[test]
    fn test_results_env_value() {
        assert_eq!(test_results_from_env(None), None);
        for off in ["", "0", "false", "off"] {
            assert_eq!(test_results_from_env(Some(off)), None, "{off}");
        }
        assert_eq!(test_results_from_env(Some("1")), Some(None));
        assert_eq!(
            test_results_from_env(Some("/tmp/r.ndjson")),
            Some(Some(std::path::PathBuf::from("/tmp/r.ndjson")))
        );
    }

    #[test]
    fn services_up_accepts_zero_or_one_group() {
        let cli = Cli::try_parse_from(["aster", "services", "up"]).unwrap();
        let Commands::Services {
            command: ServicesCommands::Up { group, .. },
        } = cli.command.unwrap()
        else {
            panic!("expected services up command");
        };
        assert_eq!(group, None);

        let cli = Cli::try_parse_from(["aster", "services", "up", "intern"]).unwrap();
        let Commands::Services {
            command: ServicesCommands::Up { group, .. },
        } = cli.command.unwrap()
        else {
            panic!("expected services up command");
        };
        assert_eq!(group.as_deref(), Some("intern"));

        assert!(Cli::try_parse_from(["aster", "services", "up", "one", "two"]).is_err());
    }

    #[test]
    fn services_up_accepts_optional_proxy_flag() {
        let cli = Cli::try_parse_from(["aster", "services", "up", "main", "--proxy"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Services {
                command: ServicesCommands::Up {
                    group: Some(group),
                    proxy: true,
                    ..
                }
            }) if group == "main"
        ));
    }

    #[test]
    fn services_up_daemon_flags_are_explicitly_validated() {
        let cli = Cli::try_parse_from(["aster", "services", "up", "intern", "--daemon"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Services {
                command: ServicesCommands::Up {
                    group: Some(group),
                    daemon: true,
                    ..
                }
            }) if group == "intern"
        ));
        assert!(Cli::try_parse_from(["aster", "services", "up", "--daemon", "--dry-run"]).is_err());
        assert!(Cli::try_parse_from(["aster", "services", "up", "--daemon", "--no-ui"]).is_err());
    }

    #[test]
    fn services_list_down_and_daemon_have_strict_arity() {
        assert!(matches!(
            Cli::try_parse_from(["aster", "services", "list"])
                .unwrap()
                .command,
            Some(Commands::Services {
                command: ServicesCommands::List
            })
        ));
        assert!(Cli::try_parse_from(["aster", "services", "list", "extra"]).is_err());

        for (args, expected) in [
            (vec!["aster", "services", "down"], None),
            (vec!["aster", "services", "down", "intern"], Some("intern")),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Some(Commands::Services {
                command: ServicesCommands::Down { group },
            }) = cli.command
            else {
                panic!("expected services down command");
            };
            assert_eq!(group.as_deref(), expected);
        }
        assert!(Cli::try_parse_from(["aster", "services", "down", "one", "two"]).is_err());

        assert!(matches!(
            Cli::try_parse_from(["aster", "services", "daemon", "stop"])
                .unwrap()
                .command,
            Some(Commands::Services {
                command: ServicesCommands::Daemon {
                    command: ServicesDaemonCommands::Stop
                }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["aster", "services", "daemon", "serve"])
                .unwrap()
                .command,
            Some(Commands::Services {
                command: ServicesCommands::Daemon {
                    command: ServicesDaemonCommands::Serve
                }
            })
        ));
        assert!(Cli::try_parse_from(["aster", "services", "daemon", "stop", "extra"]).is_err());
        let help = Cli::try_parse_from(["aster", "services", "daemon", "--help"])
            .err()
            .expect("help exits through clap")
            .to_string();
        assert!(!help.contains("serve"));
    }

    #[test]
    fn services_logs_requires_exactly_one_service() {
        let cli = Cli::try_parse_from(["aster", "services", "logs", "api"]).unwrap();
        let Commands::Services {
            command: ServicesCommands::Logs { service },
        } = cli.command.unwrap()
        else {
            panic!("expected services logs command");
        };
        assert_eq!(service, "api");

        assert!(Cli::try_parse_from(["aster", "services", "logs"]).is_err());
        assert!(Cli::try_parse_from(["aster", "services", "logs", "api", "web"]).is_err());
    }

    #[test]
    fn services_ports_accepts_no_arguments() {
        let cli = Cli::try_parse_from(["aster", "services", "ports"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Services {
                command: ServicesCommands::Ports
            })
        ));

        assert!(Cli::try_parse_from(["aster", "services", "ports", "web"]).is_err());
    }

    #[test]
    fn skills_is_available_without_a_subcommand() {
        let cli = Cli::try_parse_from(["aster", "--skills"]).unwrap();
        assert!(cli.skills);
        assert!(cli.command.is_none());

        assert!(Cli::try_parse_from(["aster"]).is_err());
    }

    #[test]
    fn help_subcommand_recognizes_only_conventional_targets() {
        let args = |tokens: &[&str]| {
            tokens
                .iter()
                .map(|token| (*token).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            conventional_target_help_name(&args(&["help", "test"])),
            Some("test")
        );
        assert_eq!(
            conventional_target_help_name(&args(&["--quiet", "help", "build"])),
            Some("build")
        );
        assert!(conventional_target_help_name(&args(&["help", "nonexistent"])).is_none());
        assert!(conventional_target_help_name(&args(&["help", "list"])).is_none());
        assert!(conventional_target_help_name(&args(&["help"])).is_none());
        assert!(external_target_requests_help(&args(&["test", "--help"])));
        assert!(external_target_requests_help(&args(&["build", "-h"])));
        assert!(!external_target_requests_help(&args(&["test", "--all"])));
        assert_eq!(
            external_target_name(&args(&["test", "--help"])),
            Some("test")
        );
    }

    #[test]
    fn explicit_target_help_names_the_target_and_leaves_builtin_help() {
        let args = |tokens: &[&str]| {
            tokens
                .iter()
                .map(|token| (*token).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            explicit_target_help_name(&args(&["target", "build", "--help"])),
            Some("build")
        );
        assert_eq!(
            explicit_target_help_name(&args(&["target", "build", "-h"])),
            Some("build")
        );
        assert_eq!(
            explicit_target_help_name(&args(&["--quiet", "target", "list", "--help"])),
            Some("list")
        );
        assert_eq!(
            explicit_target_help_name(&args(&["target", "--jobs", "4", "build", "--help"])),
            Some("build")
        );
        assert!(explicit_target_help_name(&args(&["target", "--help"])).is_none());
        assert!(explicit_target_help_name(&args(&["target", "build"])).is_none());
        assert!(explicit_target_help_name(&args(&["check", "--help"])).is_none());
        assert!(explicit_target_help_name(&args(&["build", "--help"])).is_none());
        // Help before the target name belongs to clap, including when it is a flag value.
        assert!(explicit_target_help_name(&args(&["--help", "target", "build"])).is_none());
        assert!(explicit_target_help_name(&args(&["-h", "target", "build"])).is_none());
        assert!(explicit_target_help_name(&args(&["target", "--help", "build"])).is_none());
        assert!(
            explicit_target_help_name(&args(&["--jobs", "--help", "target", "build"])).is_none()
        );
    }
}
