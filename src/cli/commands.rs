//! CLI command definitions using clap derive
//!
//! Defines the main CLI structure and available subcommands.

use clap::{Parser, Subcommand};

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

/// Available commands
#[derive(Subcommand)]
pub enum Commands {
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
    Clear {
        /// Specific target to clear (e.g., //services/api:test)
        /// If not specified, clears all cache
        target: Option<String>,
    },

    /// Show cache status
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
}
