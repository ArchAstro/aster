//! Parallel command execution with grouped output
//!
//! Executes commands in dependency order:
//! - Collects all target dependencies transitively (e.g., test depends on deps)
//! - A target starts as soon as every dependency has finished, without waiting
//!   for unrelated targets (the run takes as long as its critical path)
//! - Targets sharing an `exclusive_resources` name never run at the same time
//! - DAG levels (level 0: no dependencies; level N: dependencies in levels
//!   0..N-1) are a display concept: dry-run plans, result ordering and the
//!   tie-break between targets that become ready together

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use console::style;

use crate::cache::{
    pack, project_of, prune_if_over_cap, resolve_cache_dir, restore, ArtifactCache, CacheHasher,
    Digest, Manifest, PruneLimits, RuntimeInputCache, MANIFEST_SCHEMA_VERSION,
};
use crate::cli::OutputMode;
use crate::config::CacheConfig;
use crate::discovery::DiscoveredProject;
use crate::executor::logs::{LogStore, RunLog, TargetLog};
use crate::graph::ProjectGraph;
use crate::plugins::{LanguagePlugin, PluginRegistry};
use crate::test_results::quarantine::{Quarantine, Verdict};
use crate::test_results::{
    self, Confidence, OutputRewrite, ReportContext, ReportFiles, ReportPlan, TestReport,
};
use crate::ui::ProgressDisplay;

mod partitions;
pub use partitions::partitioned_suites;
mod retry;

use super::command::parse_command;
use super::shutdown_requested;
#[cfg(any(unix, windows))]
use super::{register_supervised_child, unregister_supervised_child};

/// Evict old entries when the local artifact store is over its cap.
///
/// Called at the end of a cached run. A lock or IO failure is a warning:
/// eviction must not fail the build. Limits come from `[cache]` in the root
/// aster.toml, defaulting to 10 GB and 14 days. Size eviction runs only when
/// the store is already over the cap; that pass also applies the age limit.
fn auto_prune_cache(workspace_root: &Path, output_mode: OutputMode) {
    let loaded = match crate::config::WorkspaceConfig::load(workspace_root) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[aster] Warning: cache prune skipped, workspace config: {error:#}");
            return;
        }
    };
    let limits = match PruneLimits::from_config(
        loaded.cache.max_size.as_deref(),
        loaded.cache.max_age.as_deref(),
    ) {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("[aster] Warning: cache prune skipped, invalid [cache]: {error:#}");
            return;
        }
    };
    match prune_if_over_cap(workspace_root, limits) {
        Ok(Some(stats)) if output_mode != OutputMode::Quiet => {
            eprintln!(
                "[aster] Pruned {} manifests and {} blobs ({} bytes remaining)",
                stats.manifests_removed, stats.blobs_removed, stats.bytes_remaining
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[aster] Warning: cache prune failed: {error:#}");
        }
    }
}

/// Replacement for a target's command, used by `aster affected
/// --only-affected-files` and `--warnings-as-errors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOverride {
    /// Run these commands in order instead of the target's command; the
    /// target fails at the first command that fails.
    Run(Vec<String>),
    /// Do not run the target; it reports as skipped with this reason.
    Skip(String),
}

/// Result of executing a target command on a project
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Target address (//path/to/project:target)
    pub address: String,
    /// Whether the command succeeded (exit code 0)
    pub success: bool,
    /// Whether the project was skipped (no target defined)
    pub skipped: bool,
    /// Whether execution was skipped due to cache hit
    pub cached: bool,
    /// Combined stdout+stderr output, the lossy UTF-8 view of [`Self::log`]
    pub output: String,
    /// Combined stdout+stderr bytes, in arrival order, capped by the executor
    pub log: Vec<u8>,
    /// Execution duration in milliseconds
    pub duration_ms: u128,
    /// Exit code of the last process that ran.
    ///
    /// `Some` whenever a process ran, including a signal death reported as
    /// `128 + signal`. `None` when no process ran: skipped, cached, blocked,
    /// or a spawn/parse failure.
    pub exit_code: Option<i32>,
    /// Per-test results, when test capture was on and the plugin could
    /// capture this target's runner
    pub tests: Option<TestReport>,
    /// Hash of everything the target's tests depend on, computed when test
    /// capture is on. Two runs with the same hash ran the same tests on the
    /// same inputs.
    pub input_hash: Option<String>,
}

/// Executor for running target commands on projects
pub struct Executor<'a> {
    /// Workspace root directory
    workspace_root: &'a Path,
    /// Output mode for controlling what gets printed
    output_mode: OutputMode,
    /// Whether to show full logs for failures instead of truncated output
    full_logs: bool,
    /// Whether to use caching
    use_cache: bool,
    /// Directory holding `cas/`, `ac/`, `index/`, and `cache.lock`, resolved once.
    ///
    /// A legacy `cache.json` in this directory is obsolete and unused.
    cache_dir: PathBuf,
    /// Whether child targets should receive a closed stdin.
    ///
    /// Captured execution always uses a closed stdin so tools that prompt on a
    /// TTY fail instead of hanging behind Aster's progress UI. Streaming
    /// targets inherit the caller's terminal separately.
    null_stdin: bool,
    /// Whether to capture per-test results from targets whose runner the
    /// plugin knows how to report on.
    capture_tests: bool,
    /// How many times to rerun a failed target's failing tests. Test
    /// capture is planned for every target when this is above 0, whether or
    /// not results are recorded.
    retry_failed: u32,
    /// Maximum combined stdout+stderr bytes retained for one command.
    log_cap: usize,
    /// The tests whose failures may not fail their target.
    quarantine: Option<Arc<Quarantine>>,
    /// When set, a cache hit still reports `cached` but does not print the
    /// stored log. Status and JSON fields are unchanged.
    no_replay: bool,
}

/// Default cap on one command's combined log: 16 MiB.
pub const DEFAULT_LOG_CAP_BYTES: usize = 16 * 1024 * 1024;

impl<'a> Executor<'a> {
    /// Create a new executor with default output mode (Normal)
    pub fn new(workspace_root: &'a Path) -> Self {
        Self {
            workspace_root,
            output_mode: OutputMode::Normal,
            full_logs: false,
            use_cache: true,
            cache_dir: resolve_cache_dir(workspace_root),
            null_stdin: true,
            capture_tests: false,
            retry_failed: 0,
            log_cap: DEFAULT_LOG_CAP_BYTES,
            quarantine: None,
            no_replay: false,
        }
    }

    /// Create a new executor with specified output mode
    pub fn with_output_mode(workspace_root: &'a Path, output_mode: OutputMode) -> Self {
        Self {
            workspace_root,
            output_mode,
            full_logs: false,
            use_cache: true,
            cache_dir: resolve_cache_dir(workspace_root),
            null_stdin: true,
            capture_tests: false,
            retry_failed: 0,
            log_cap: DEFAULT_LOG_CAP_BYTES,
            quarantine: None,
            no_replay: false,
        }
    }

    /// Create a new executor with specified output mode and full_logs option
    pub fn with_options(
        workspace_root: &'a Path,
        output_mode: OutputMode,
        full_logs: bool,
    ) -> Self {
        Self {
            workspace_root,
            output_mode,
            full_logs,
            use_cache: true,
            cache_dir: resolve_cache_dir(workspace_root),
            null_stdin: true,
            capture_tests: false,
            retry_failed: 0,
            log_cap: DEFAULT_LOG_CAP_BYTES,
            quarantine: None,
            no_replay: false,
        }
    }

    /// Create a new executor with all options including cache control
    pub fn with_all_options(
        workspace_root: &'a Path,
        output_mode: OutputMode,
        full_logs: bool,
        use_cache: bool,
    ) -> Self {
        Self {
            workspace_root,
            output_mode,
            full_logs,
            use_cache,
            cache_dir: resolve_cache_dir(workspace_root),
            null_stdin: true,
            capture_tests: false,
            retry_failed: 0,
            log_cap: DEFAULT_LOG_CAP_BYTES,
            quarantine: None,
            no_replay: false,
        }
    }

    /// Prevent targets from reading the caller's terminal.
    ///
    /// Captured execution already closes stdin. This is kept so call sites such
    /// as dev-service prerequisites can document that the child must not share
    /// the dashboard's terminal.
    /// Cap the combined stdout+stderr bytes retained for each command.
    ///
    /// Bytes past the cap are discarded and replaced by a truncation marker.
    /// `0` keeps only the marker once any output arrives.
    pub fn with_log_cap(mut self, bytes: usize) -> Self {
        self.log_cap = bytes;
        self
    }

    /// Suppress printing a cache hit's stored log. The hit is still reported.
    pub fn with_no_replay(mut self, no_replay: bool) -> Self {
        self.no_replay = no_replay;
        self
    }

    /// Prevent targets from reading the caller's terminal.
    pub fn with_null_stdin(mut self) -> Self {
        self.null_stdin = true;
        self
    }

    /// Capture per-test results into [`ExecutionResult::tests`].
    ///
    /// A target's command is rewritten (for example to add a reporter flag)
    /// only after its cache key is computed, so capture never changes what
    /// is cached.
    pub fn with_test_capture(mut self, capture_tests: bool) -> Self {
        self.capture_tests = capture_tests;
        self
    }

    /// Rerun only the failing tests of a failed test target, up to `retries`
    /// times, with `retry_setup` run before each retry.
    pub fn with_retry_failed(mut self, retries: u32) -> Self {
        self.retry_failed = retries;
        self
    }

    /// Let a failed test target pass when, after retries, its only
    /// failures are quarantined tests (see [`crate::test_results::quarantine`]).
    /// Applies only to targets whose results are captured.
    pub fn with_quarantine(mut self, quarantine: Option<Quarantine>) -> Self {
        self.quarantine = quarantine.map(Arc::new);
        self
    }

    /// Execute a target on selected projects in dependency order
    ///
    /// Respects target dependencies from Target.depends_on (fully resolved by TargetResolver).
    /// Each target starts as soon as its dependencies have finished.
    /// Output is buffered per-target and printed as a group when complete.
    /// Independent branches continue on failure; dependent targets are blocked.
    ///
    /// The `primary_projects` set specifies which projects should run the requested target.
    /// Dependency projects are included for target-level dependency resolution (e.g., :build)
    /// but won't run the requested target unless they're in the primary set.
    pub fn execute(
        &self,
        target: &str,
        projects: &[&DiscoveredProject],
        _graph: &ProjectGraph,
        primary_projects: Option<&HashSet<String>>,
    ) -> Vec<ExecutionResult> {
        self.execute_internal(target, projects, None, primary_projects)
    }

    /// Execute a target with command overrides for specific targets
    ///
    /// The `command_overrides` map allows replacing the command for specific target addresses.
    /// This is used when --only-affected-files is enabled to pass file lists to targets.
    pub fn execute_with_command_overrides(
        &self,
        target: &str,
        projects: &[&DiscoveredProject],
        command_overrides: &HashMap<String, CommandOverride>,
        primary_projects: Option<&HashSet<String>>,
    ) -> Vec<ExecutionResult> {
        self.execute_internal(target, projects, Some(command_overrides), primary_projects)
    }

    /// Execute a target with streaming output (for long-running processes like dev servers)
    ///
    /// Output is streamed directly to stdout/stderr instead of being captured.
    /// Only supports running on a single project - returns error if multiple projects.
    /// Progress UI is disabled in streaming mode.
    pub fn execute_streaming(
        &self,
        target: &str,
        project: &DiscoveredProject,
    ) -> Result<i32, String> {
        let project_addr = format!("//{}", project.relative_path.display());
        let target_addr = format!("{project_addr}:{target}");

        // Get the target and command
        let target_def = match project.targets.get(target) {
            Some(t) => t,
            None => {
                return Err(format!("No '{target}' target defined for {project_addr}"));
            }
        };
        let command = &target_def.command;

        // Use target's working_dir if set, otherwise use project root
        let working_dir = target_def.working_dir.as_ref().unwrap_or(&project.root);

        if self.output_mode == OutputMode::Verbose {
            eprintln!("[aster] Running: {command}");
            eprintln!("[aster] Working directory: {}", working_dir.display());
        }

        let parsed = parse_command(command).map_err(|error| error.to_string())?;

        // Run with inherited stdio for streaming
        use std::process::Stdio;
        let mut cmd = Command::new(&parsed.program);
        cmd.args(&parsed.args)
            .current_dir(working_dir)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        for (name, value) in parsed.env {
            cmd.env(name, value);
        }

        #[cfg(unix)]
        isolate_streaming_session(&mut cmd);
        #[cfg(windows)]
        prepare_windows_child(&mut cmd);

        #[cfg(any(unix, windows))]
        register_supervised_child();
        let mut child = cmd.spawn().map_err(|e| {
            #[cfg(any(unix, windows))]
            unregister_supervised_child();
            format!("Failed to execute {target_addr}: {e}")
        })?;
        #[cfg(unix)]
        let monitor = ChildSignalMonitor::new(child.id());
        #[cfg(windows)]
        let monitor = WindowsChildSignalMonitor::new(&mut child)
            .inspect_err(|_| unregister_supervised_child())
            .map_err(|error| format!("Failed to supervise {target_addr}: {error}"))?;
        let status = child.wait();
        #[cfg(any(unix, windows))]
        monitor.finish();
        let status = status.map_err(|e| format!("Failed to execute {target_addr}: {e}"))?;

        Ok(status.code().unwrap_or(1))
    }

    /// Execute an explicit set of target addresses (heterogeneous run).
    ///
    /// Builds the `targets_to_run` set by expanding `primary_targets` with their
    /// transitive dependencies (unless `no_deps` is true), then runs the cached
    /// execution pipeline. Used by `aster watch` to rerun a precomputed set of
    /// targets whose inputs were touched.
    pub fn execute_targets(
        &self,
        primary_targets: &HashSet<String>,
        projects: &[&DiscoveredProject],
        no_deps: bool,
    ) -> Vec<ExecutionResult> {
        if projects.is_empty() || primary_targets.is_empty() {
            return Vec::new();
        }

        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), *p))
            .collect();

        let mut targets_to_run: HashSet<String> = primary_targets.clone();
        if !no_deps {
            for addr in primary_targets {
                collect_target_deps(addr, &project_map, &mut targets_to_run);
            }
        }

        let target_label = primary_targets
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "watch".to_string());

        self.execute_target_set(&targets_to_run, &project_map, None, &target_label)
    }

    /// Internal execution method that supports optional command overrides
    fn execute_internal(
        &self,
        target: &str,
        projects: &[&DiscoveredProject],
        command_overrides: Option<&HashMap<String, CommandOverride>>,
        primary_projects: Option<&HashSet<String>>,
    ) -> Vec<ExecutionResult> {
        if projects.is_empty() {
            return Vec::new();
        }

        // Build address -> project map (for all discovered projects, not just selected)
        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), *p))
            .collect();

        // Collect all targets to execute (requested targets + their dependencies)
        // Only run the requested target on primary projects; dependency projects are included
        // for target-level dependency resolution (e.g., :build) but won't run the requested target
        let mut targets_to_run: HashSet<String> = HashSet::new();
        for project in projects {
            let project_addr = format!("//{}", project.relative_path.display());

            // Only add the requested target for primary projects
            // If primary_projects is None, treat all projects as primary (backwards compat)
            let is_primary = primary_projects
                .map(|p| p.contains(&project_addr))
                .unwrap_or(true);

            if is_primary {
                let target_addr = format!("{project_addr}:{target}");

                // Add the requested target
                targets_to_run.insert(target_addr.clone());

                // Recursively collect target dependencies
                collect_target_deps(&target_addr, &project_map, &mut targets_to_run);
            }
        }

        self.execute_target_set(&targets_to_run, &project_map, command_overrides, target)
    }

    /// Shared pipeline: schedule `targets_to_run` by dependency readiness
    /// with caching, then finalize progress + logs.
    fn execute_target_set(
        &self,
        targets_to_run: &HashSet<String>,
        project_map: &HashMap<String, &DiscoveredProject>,
        command_overrides: Option<&HashMap<String, CommandOverride>>,
        target_label_for_logs: &str,
    ) -> Vec<ExecutionResult> {
        if targets_to_run.is_empty() {
            return Vec::new();
        }

        if self.output_mode != OutputMode::Json {
            let log_store = LogStore::new(self.workspace_root);
            if let Err(error) = log_store.clear_latest() {
                eprintln!("[aster] Warning: Failed to clear previous logs: {error}");
            }
        }

        let show_progress = matches!(self.output_mode, OutputMode::Normal | OutputMode::Verbose);
        let verbose_progress = self.output_mode == OutputMode::Verbose;
        let show_output = matches!(self.output_mode, OutputMode::Normal | OutputMode::Verbose);

        let mut progress = ProgressDisplay::with_verbose(show_progress, verbose_progress);

        progress.set_total(targets_to_run.len());

        let artifact_cache = if self.use_cache {
            Some(ArtifactCache::new(
                self.cache_dir.clone(),
                self.workspace_root,
            ))
        } else {
            None
        };
        let plugin_registry = PluginRegistry::with_all_plugins();

        let computed_hashes: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let input_hashes: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());

        let env_snapshot: HashMap<String, String> = std::env::vars().collect();
        let runtime_inputs = Arc::new(RuntimeInputCache::new(self.workspace_root));

        let levels = compute_target_levels(targets_to_run, project_map);
        let level_of: HashMap<String, usize> = levels
            .iter()
            .enumerate()
            .flat_map(|(level, addrs)| addrs.iter().map(move |addr| (addr.clone(), level)))
            .collect();
        let mut queue = ReadyQueue::new(targets_to_run, project_map, &level_of);

        // One lock per exclusive resource for the whole run: targets that
        // share a resource serialize even when they sit at different levels.
        let resource_mutexes = exclusive_resource_locks(targets_to_run, project_map);
        let (tx, rx) = mpsc::channel();
        let ctx = LaunchContext {
            project_map,
            command_overrides,
            cache_store: artifact_cache.as_ref(),
            plugin_registry: &plugin_registry,
            computed_hashes: &computed_hashes,
            input_hashes: &input_hashes,
            env_snapshot: &env_snapshot,
            runtime_inputs: &runtime_inputs,
            resource_mutexes: &resource_mutexes,
            tx: &tx,
        };

        // (level, rank, result): rank 0 for blocked targets, 1 for the rest,
        // so the final order groups by level and lists blocked targets first.
        let mut collected: Vec<(usize, u8, ExecutionResult)> = Vec::new();
        let mut failed_or_blocked: HashSet<String> = HashSet::new();
        let mut handles = Vec::new();
        let mut running = 0usize;

        // A target starts the moment its last dependency finishes. The loop
        // blocks on the result channel whenever nothing new can start.
        loop {
            let stopping = shutdown_requested();
            let mut batch = if stopping {
                Vec::new()
            } else {
                queue.take_ready()
            };
            if batch.is_empty() && running == 0 && !stopping {
                // Nothing runs and nothing is ready: a dependency cycle.
                batch = queue.take_stuck();
            }
            if batch.is_empty() {
                if running == 0 {
                    break;
                }
                let Ok(result) = rx.recv() else { break };
                running -= 1;
                if show_progress && !result.cached {
                    if result.success {
                        let flaky = result.tests.as_ref().map_or(0, |t| t.flaky().len());
                        if flaky > 0 {
                            progress.mark_flaky(&result.address, flaky);
                        }
                        let quarantined = result.tests.as_ref().map_or(0, |t| t.quarantined.len());
                        if quarantined > 0 {
                            progress.mark_quarantined(&result.address, quarantined);
                        }
                    }
                    progress.mark_complete(
                        &result.address,
                        result.success,
                        result.skipped,
                        result.duration_ms,
                    );
                }
                if !result.success {
                    failed_or_blocked.insert(result.address.clone());
                }
                queue.complete(&result.address);
                collected.push((queue.level(&result.address), 1, result));
                continue;
            }

            if let Some(cache) = artifact_cache.as_ref() {
                let mut memo = HashMap::new();
                if let Ok(guard) = computed_hashes.lock() {
                    memo.extend(
                        guard
                            .iter()
                            .map(|(addr, key)| (addr.clone(), Some(key.clone()))),
                    );
                }
                let keys: Vec<String> = batch
                    .iter()
                    .filter_map(|addr| {
                        fresh_cache_key(
                            addr,
                            project_map,
                            self.workspace_root,
                            &plugin_registry,
                            &env_snapshot,
                            &runtime_inputs,
                            &mut memo,
                        )
                    })
                    .collect();
                cache.prefetch(&keys);
            }

            for target_addr in batch {
                let failed_dependencies = target_dependencies(&target_addr, project_map)
                    .into_iter()
                    .filter(|dependency| failed_or_blocked.contains(dependency))
                    .collect::<Vec<_>>();

                if failed_dependencies.is_empty() {
                    if self.launch_target(
                        &target_addr,
                        &ctx,
                        &mut progress,
                        show_progress,
                        &mut handles,
                    ) {
                        running += 1;
                    } else {
                        queue.complete(&target_addr);
                    }
                    continue;
                }

                if show_progress {
                    progress.add_running(&target_addr);
                    progress.mark_complete(&target_addr, false, false, 0);
                }
                failed_or_blocked.insert(target_addr.clone());
                queue.complete(&target_addr);
                let level = queue.level(&target_addr);
                collected.push((
                    level,
                    0,
                    ExecutionResult {
                        address: target_addr,
                        success: false,
                        skipped: false,
                        cached: false,
                        output: format!(
                            "Blocked: prerequisite target(s) failed: {}",
                            failed_dependencies.join(", ")
                        ),
                        log: Vec::new(),
                        duration_ms: 0,
                        exit_code: None,
                        tests: None,
                        input_hash: None,
                    },
                ));
            }
        }

        for handle in handles {
            let _ = handle.join();
        }
        collected.sort_by_key(|(level, rank, _)| (*level, *rank));
        let all_results: Vec<ExecutionResult> =
            collected.into_iter().map(|(_, _, result)| result).collect();

        progress.finish();

        if self.output_mode != OutputMode::Json {
            self.store_logs(target_label_for_logs, &all_results);
        }

        if show_output {
            self.print_failure_details(&all_results, &progress);
        }

        if self.use_cache {
            auto_prune_cache(self.workspace_root, self.output_mode);
            if let Some(backend) = crate::cache::current_gha() {
                // A failed pack upload drops the manifests held for it, so
                // no stored manifest names missing blobs; those targets are
                // remote misses next run, not a failed build.
                if let Err(error) = backend.finish() {
                    eprintln!("[aster] Warning: remote cache upload failed; targets bundled into the pack were not published: {error:#}");
                }
                let summary = backend.summary();
                eprintln!("{summary}");
            } else if let Some(summary) = crate::cache::RemoteStats::global().take() {
                eprintln!("{summary}");
            }
        }

        all_results
    }

    /// Start one target: answer it immediately (skipped, cache hit) or spawn
    /// the thread that runs it. Every outcome is delivered on `ctx.tx`.
    ///
    /// Returns whether a result will arrive on `ctx.tx`. It is `false` only
    /// for an address that names no known project.
    fn launch_target(
        &self,
        target_addr: &str,
        ctx: &LaunchContext<'_>,
        progress: &mut ProgressDisplay,
        show_progress: bool,
        handles: &mut Vec<thread::JoinHandle<()>>,
    ) -> bool {
        let LaunchContext {
            project_map,
            command_overrides,
            cache_store,
            plugin_registry,
            computed_hashes,
            input_hashes,
            env_snapshot,
            runtime_inputs,
            resource_mutexes,
            tx: _,
        } = *ctx;
        let target_addr = &target_addr.to_string();
        // Parse target address: //path/to/project:target_name
        let (project_addr, target_name) = match parse_target_address(target_addr) {
            Some((p, t)) => (p, t),
            None => return false,
        };

        let project = match project_map.get(&project_addr) {
            Some(p) => *p,
            None => return false,
        };

        // Get the command for this target
        // Check for command override first (used with --only-affected-files)
        let commands: Vec<String> = if let Some(overrides) = command_overrides {
            if let Some(command_override) = overrides.get(target_addr) {
                match command_override {
                    CommandOverride::Run(commands) => commands.clone(),
                    CommandOverride::Skip(reason) => {
                        let result = ExecutionResult {
                            address: target_addr.clone(),
                            success: true,
                            skipped: true,
                            cached: false,
                            output: format!("Skipped: {reason}"),
                            log: Vec::new(),
                            duration_ms: 0,
                            exit_code: None,
                            tests: None,
                            input_hash: None,
                        };
                        if show_progress {
                            progress.mark_skipped(target_addr);
                        }
                        let _ = ctx.tx.send(result);
                        return true;
                    }
                }
            } else {
                match project.targets.get(&target_name) {
                    Some(t) => vec![t.command.clone()],
                    None => {
                        // No target defined - skip
                        let result = ExecutionResult {
                            address: target_addr.clone(),
                            success: true,
                            skipped: true,
                            cached: false,
                            output: format!("Skipped: no '{target_name}' target defined"),
                            log: Vec::new(),
                            duration_ms: 0,
                            exit_code: None,
                            tests: None,
                            input_hash: None,
                        };
                        if show_progress {
                            progress.mark_skipped(target_addr);
                        }
                        let _ = ctx.tx.send(result);
                        return true;
                    }
                }
            }
        } else {
            match project.targets.get(&target_name) {
                Some(t) => vec![t.command.clone()],
                None => {
                    // No target defined - skip
                    let result = ExecutionResult {
                        address: target_addr.clone(),
                        success: true, // Not an error, just skipped
                        skipped: true,
                        cached: false,
                        output: format!("Skipped: no '{target_name}' target defined"),
                        log: Vec::new(),
                        duration_ms: 0,
                        exit_code: None,
                        tests: None,
                        input_hash: None,
                    };
                    if show_progress {
                        progress.mark_skipped(target_addr);
                    }
                    let _ = ctx.tx.send(result);
                    return true;
                }
            }
        };
        // Cache keys and logs see one command per line.
        let command = commands.join("\n");
        let excluded_dirs = nested_project_roots(project, project_map);
        let input_hash = if self.capture_tests {
            test_input_hash(
                target_addr,
                &target_name,
                &command,
                project,
                self.workspace_root,
                project_map,
                plugin_registry,
                input_hashes,
                env_snapshot,
                &excluded_dirs,
                runtime_inputs,
            )
        } else {
            None
        };

        // Check cache if enabled. A miss carries the exact hash that may
        // be stored after successful execution; `None` means this target
        // is not cacheable and must not create a cache entry.
        let mut cache_hash = None;
        if let Some(store) = cache_store {
            if let Some(lookup) = check_cache(
                target_addr,
                &target_name,
                &command,
                project,
                self.workspace_root,
                store,
                plugin_registry,
                computed_hashes,
                env_snapshot,
                &excluded_dirs,
                runtime_inputs,
            ) {
                match lookup {
                    CacheLookup::Hit { manifest, log } => {
                        let result = cached_result(target_addr, &manifest, log, input_hash.clone());
                        // Same channel as live captured output: `aster logs`
                        // prints `ExecutionResult.output`, and a hit replays
                        // that stored log here unless `--no-replay` is set.
                        // JSON keeps the log on the result and does not print it.
                        if self.output_mode != OutputMode::Json && !self.no_replay {
                            print_replayed_log(&result.output);
                        }
                        if show_progress {
                            progress.mark_cached(target_addr, result.duration_ms);
                        }
                        let _ = ctx.tx.send(result);
                        return true;
                    }
                    CacheLookup::Miss(current_hash) => {
                        cache_hash = Some(current_hash);
                    }
                }
            } else if let Err(e) = store.remove(target_addr) {
                // A disabled or currently unhashable target must not leave
                // an older entry available for a future false hit.
                eprintln!(
                    "[aster] Warning: Failed to remove unavailable cache for {target_addr}: {e}"
                );
            }
        }

        // Add spinner for this target
        if show_progress {
            progress.add_running(target_addr);
        }

        let addr = target_addr.clone();
        let project_root = project.root.clone();
        // Use target's working_dir if set, otherwise use project root
        let working_dir = project
            .targets
            .get(&target_name)
            .and_then(|t| t.working_dir.clone())
            .unwrap_or_else(|| project_root.clone());
        // A partition of a suite runs the share recorded durations give
        // it. Like test capture, this rewrites the commands only after
        // the cache key and input hash were computed from them as
        // written.
        let commands = match partitions::balance(
            target_addr,
            &commands,
            &working_dir,
            self.workspace_root,
            &|name| std::env::var(name).ok(),
        ) {
            Some(balanced) => vec![balanced],
            None => commands,
        };
        // From the command as configured, so an override that narrows
        // it never changes whose tests these are.
        let test_project = project.targets.get(&target_name).and_then(|target| {
            partitions::test_owner(project, target, &target.command, project_map)
        });
        // Test capture rewrites the commands only now, after the cache
        // key was computed from the commands as written.
        let captures = if self.capture_tests || self.retry_failed > 0 {
            plugin_registry
                .find_by_name(&project.plugin_name)
                .map(|plugin| {
                    let declared = project
                        .targets
                        .get(&target_name)
                        .and_then(|target| target.report.as_ref());
                    plan_captures(
                        plugin,
                        declared,
                        &project_root,
                        &working_dir,
                        &target_name,
                        &commands,
                    )
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let invocations: Vec<Invocation> = commands
            .iter()
            .enumerate()
            .map(|(index, command)| match captures.get(index) {
                Some(Some((plan, _))) => Invocation::planned(command, plan),
                _ => Invocation::plain(command),
            })
            .collect();
        let excluded_dirs_for_rehash = excluded_dirs.clone();
        let tx_clone = ctx.tx.clone();
        let computed_hashes_clone = Arc::clone(computed_hashes);
        let cache_store_path = cache_store.map(|_| self.cache_dir.clone());
        let workspace_root_path = self.workspace_root.to_path_buf();
        let command_clone = command.clone();
        let target_name_clone = target_name.clone();
        let plugin_name = project.plugin_name.clone();
        let env_snapshot_clone = env_snapshot.clone();
        let runtime_inputs_clone = Arc::clone(runtime_inputs);
        let target_deps = project
            .targets
            .get(&target_name)
            .map(|target| target.depends_on.clone())
            .unwrap_or_default();
        let target_cache_config = project
            .targets
            .get(&target_name)
            .and_then(|target| target.cache.clone());
        // Get invalidates_cache flag for cache invalidation after execution
        let invalidates_cache = project
            .targets
            .get(&target_name)
            .map(|t| t.invalidates_cache)
            .unwrap_or(false);
        let null_stdin = self.null_stdin;
        let log_cap = self.log_cap;
        // Retrying reruns one command, so a target that runs several
        // (a split `--only-affected-files` run) is not retried.
        let retry_command =
            (self.retry_failed > 0 && commands.len() == 1).then(|| commands[0].clone());
        let retry_failed = self.retry_failed;
        // Quarantine judges one command's results, like a retry.
        let quarantine = self
            .quarantine
            .clone()
            .filter(|_| !captures.is_empty() && commands.len() == 1);
        let quarantine_command = commands.first().cloned();
        let retry_setup = project
            .targets
            .get(&target_name)
            .and_then(|target| target.retry_setup.clone());
        // Collect per-resource mutexes this target needs to acquire
        let target_resource_locks: Vec<Arc<Mutex<()>>> = {
            let mut res_names: Vec<&String> = project
                .targets
                .get(&target_name)
                .map(|t| t.exclusive_resources.iter().collect())
                .unwrap_or_default();
            // Sort by resource name to prevent deadlocks
            res_names.sort();
            res_names
                .iter()
                .filter_map(|name| resource_mutexes.get(*name).map(Arc::clone))
                .collect()
        };

        let panic_addr = addr.clone();
        let panic_tx = ctx.tx.clone();
        let handle = thread::spawn(move || {
            let run = move || {
                #[cfg(test)]
                if test_hooks::should_panic(&addr) {
                    panic!("injected panic for {addr}");
                }
                // Acquire exclusive resource locks (sorted to prevent deadlocks)
                let _resource_guards: Vec<_> = target_resource_locks
                    .iter()
                    .map(|m| m.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
                    .collect();

                if shutdown_requested() {
                    let result = ExecutionResult {
                        address: addr,
                        success: false,
                        skipped: true,
                        cached: false,
                        output: "Cancelled: shutdown requested".to_string(),
                        log: Vec::new(),
                        duration_ms: 0,
                        exit_code: None,
                        tests: None,
                        input_hash: None,
                    };
                    let _ = tx_clone.send(result.clone());
                    return result;
                }
                let started = SystemTime::now();
                let (mut result, raw_outputs) =
                    run_invocations(&addr, &invocations, &working_dir, null_stdin, log_cap);
                if !captures.is_empty() {
                    result.tests = read_test_report(&addr, &captures, &raw_outputs, started);
                }
                let mut retried = false;
                if let (Some(command), false) = (&retry_command, shutdown_requested()) {
                    let registry = PluginRegistry::with_all_plugins();
                    if let Some(plugin) = registry.find_by_name(&plugin_name) {
                        retried = retry::rerun_failures(
                            &retry::RetryJob {
                                address: &addr,
                                plugin,
                                project_dir: &project_root,
                                working_dir: &working_dir,
                                target_name: &target_name_clone,
                                command,
                                setup: retry_setup.as_deref(),
                                max_retries: retry_failed,
                                null_stdin,
                                log_cap,
                            },
                            &mut result,
                        );
                    }
                }
                if let Some(report) = &mut result.tests {
                    report.project = test_project;
                }
                if let Some(report) = &result.tests {
                    if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                        for line in
                            retry::flaky_annotations(report, &working_dir, &workspace_root_path)
                        {
                            eprintln!("{line}");
                        }
                    }
                }
                let mut let_through = false;
                if let (Some(quarantine), Some(command), Some(report), false) = (
                    &quarantine,
                    &quarantine_command,
                    result.tests.as_ref(),
                    result.success || result.skipped || shutdown_requested(),
                ) {
                    match quarantine.judge(&addr, &plugin_name, command, result.exit_code, report) {
                        Verdict::Pass(tests) => {
                            let mut note = format!(
                            "[aster] Quarantine: every failing test is quarantined, so {addr} passes\n"
                        );
                            for test in &tests {
                                note.push_str(&format!(
                                    "[aster]   {} ({})\n",
                                    test.name, test.source
                                ));
                            }
                            retry::append_log(&mut result, &note, &[]);
                            if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                                for line in retry::quarantine_annotations(
                                    &tests,
                                    &working_dir,
                                    &workspace_root_path,
                                ) {
                                    eprintln!("{line}");
                                }
                            }
                            if let Some(report) = result.tests.as_mut() {
                                report.quarantined = tests;
                            }
                            result.success = true;
                            let_through = true;
                        }
                        Verdict::Fail(why) => {
                            retry::append_log(
                                &mut result,
                                &format!("[aster] Quarantine: {why}\n"),
                                &[],
                            );
                        }
                        Verdict::Untouched => {}
                    }
                }
                result.input_hash = input_hash;
                if shutdown_requested() {
                    result.success = false;
                    result.skipped = true;
                    result.output = if result.output.is_empty() {
                        "Cancelled: shutdown requested".to_string()
                    } else {
                        format!("{}\nCancelled: shutdown requested", result.output)
                    };
                }

                // A pass that needed a retry or the quarantine is not a
                // clean pass to replay from the cache: the next run should
                // see the flake again. Failures are never stored.
                let clean_pass =
                    result.success && result.exit_code == Some(0) && !retried && !let_through;
                if !clean_pass {
                    if let Some(cache_dir) = cache_store_path.as_ref() {
                        let store = ArtifactCache::new(cache_dir.clone(), &workspace_root_path);
                        if let Err(e) = store.remove(&addr) {
                            eprintln!(
                            "[aster] Warning: Failed to remove cache after failure for {addr}: {e}"
                        );
                        }
                    }
                } else if let (Some(cache_dir), Some(preflight_hash)) =
                    (cache_store_path.as_ref(), cache_hash)
                {
                    let plugin_registry = PluginRegistry::with_all_plugins();
                    if let Some(plugin) = plugin_registry.find_by_name(&plugin_name) {
                        let plugin_inputs = cache_inputs_for(
                            plugin,
                            &project_root,
                            &workspace_root_path,
                            &target_name_clone,
                        );
                        let hasher = CacheHasher::new(&project_root, &workspace_root_path)
                            .with_excluded_dirs(excluded_dirs_for_rehash);
                        let hashes_guard = computed_hashes_clone.lock().unwrap();
                        let dependency_hashes = target_deps
                            .iter()
                            .filter_map(|dependency| {
                                hashes_guard.get(dependency).map(String::as_str)
                            })
                            .collect::<Vec<_>>();
                        let postflight_hash = hasher.compute_hash(
                            &plugin_inputs,
                            target_cache_config.as_ref(),
                            &command_clone,
                            &dependency_hashes,
                            &env_snapshot_clone,
                            &runtime_inputs_clone,
                        );
                        drop(hashes_guard);

                        match postflight_hash {
                            Ok(postflight_hash) if postflight_hash == preflight_hash => {
                                {
                                    let mut hashes = computed_hashes_clone.lock().unwrap();
                                    hashes.insert(addr.clone(), postflight_hash.clone());
                                }
                                let artifacts =
                                    ArtifactCache::new(cache_dir.clone(), &workspace_root_path);
                                if let Err(e) = store_success(
                                    &artifacts,
                                    &project_root,
                                    &addr,
                                    &postflight_hash,
                                    target_cache_config.as_ref(),
                                    &result,
                                ) {
                                    eprintln!(
                                        "[aster] Warning: Failed to update cache for {addr}: {e}"
                                    );
                                }
                            }
                            _ => {
                                // An unstable or unhashable execution may
                                // have changed outputs. Do not leave an old
                                // entry available for a future false hit.
                                let store =
                                    ArtifactCache::new(cache_dir.clone(), &workspace_root_path);
                                if let Err(e) = store.remove(&addr) {
                                    eprintln!(
                                        "[aster] Warning: Failed to remove stale cache for {addr}: {e}"
                                    );
                                }
                            }
                        }
                    }
                }

                // Invalidate project cache if target has invalidates_cache flag
                if result.success && invalidates_cache {
                    if let Some(ref cache_dir) = cache_store_path {
                        let store = ArtifactCache::new(cache_dir.clone(), &workspace_root_path);
                        let project_addr = project_of(&addr).unwrap_or(&addr);
                        if let Err(e) = store.invalidate_project(project_addr) {
                            eprintln!(
                            "[aster] Warning: Failed to invalidate cache for {project_addr}: {e}"
                        );
                        }
                    }
                }

                let _ = tx_clone.send(result.clone());
                result
            };
            // A panic must still report a result: the scheduler counts
            // results to know when the run is over.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err() {
                let _ = panic_tx.send(ExecutionResult {
                    address: panic_addr,
                    success: false,
                    skipped: false,
                    cached: false,
                    output: "Internal error: target runner panicked".to_string(),
                    log: Vec::new(),
                    duration_ms: 0,
                    exit_code: None,
                    tests: None,
                    input_hash: None,
                });
            }
        });

        handles.push(handle);
        true
    }

    /// Store execution logs to .aster/logs/latest.json
    fn store_logs(&self, target: &str, results: &[ExecutionResult]) {
        let log_store = LogStore::new(self.workspace_root);

        let target_logs: Vec<TargetLog> = results
            .iter()
            .map(|r| TargetLog {
                address: r.address.clone(),
                status: if r.skipped {
                    "skipped".to_string()
                } else if r.success {
                    "passed".to_string()
                } else {
                    "failed".to_string()
                },
                exit_code: r.exit_code,
                duration_ms: r.duration_ms,
                output: r.output.clone(),
            })
            .collect();

        let run_log = RunLog {
            timestamp: Utc::now().to_rfc3339(),
            target: target.to_string(),
            results: target_logs,
        };

        if let Err(e) = log_store.store(&run_log) {
            eprintln!("[aster] Warning: Failed to store logs: {e}");
        }
    }

    /// Print failure details for failed targets
    fn print_failure_details(&self, results: &[ExecutionResult], progress: &ProgressDisplay) {
        let failed: Vec<_> = results
            .iter()
            .filter(|r| !r.success && !r.skipped)
            .collect();

        if failed.is_empty() {
            return;
        }

        for result in failed {
            // Print blank line
            if progress.is_enabled() {
                progress.println("");
            } else {
                eprintln!();
            }

            // Print "FAILED //project:target" in red
            let header = format!("{} {}", style("FAILED").red().bold(), result.address);
            if progress.is_enabled() {
                progress.println(&header);
            } else {
                eprintln!("{header}");
            }

            // Print output (full or truncated based on full_logs flag)
            let lines: Vec<&str> = result.output.lines().collect();
            let output_lines = if self.full_logs {
                // Full output mode - show all lines
                &lines[..]
            } else {
                // Truncated mode - show last 15 lines
                if lines.len() > 15 {
                    &lines[lines.len() - 15..]
                } else {
                    &lines[..]
                }
            };

            for line in output_lines {
                let indented = format!("    {line}");
                if progress.is_enabled() {
                    progress.println(&indented);
                } else {
                    eprintln!("{indented}");
                }
            }

            // Print hint for full output (only if not already showing full logs)
            if !self.full_logs {
                let hint = format!(
                    "    {}",
                    style(format!(
                        "Run `aster logs {}` for full output",
                        result.address
                    ))
                    .dim()
                );
                if progress.is_enabled() {
                    progress.println(&hint);
                } else {
                    eprintln!("{hint}");
                }
            }
        }
    }
}

fn target_dependencies(
    target_addr: &str,
    project_map: &HashMap<String, &DiscoveredProject>,
) -> Vec<String> {
    let Some((project_addr, target_name)) = parse_target_address(target_addr) else {
        return Vec::new();
    };
    project_map
        .get(&project_addr)
        .and_then(|project| project.targets.get(&target_name))
        .map(|target| target.depends_on.clone())
        .unwrap_or_default()
}

/// Parse a target address like "//path/to/project:target" into (project_addr, target_name)
pub fn parse_target_address(addr: &str) -> Option<(String, String)> {
    let colon_pos = addr.rfind(':')?;
    let project_addr = addr[..colon_pos].to_string();
    let target_name = addr[colon_pos + 1..].to_string();
    Some((project_addr, target_name))
}

/// Recursively collect all target dependencies for a given target
///
/// Follows Target.depends_on which should already be fully resolved by TargetResolver
/// (including cross-project :build dependencies).
pub fn collect_target_deps(
    target_addr: &str,
    project_map: &HashMap<String, &DiscoveredProject>,
    collected: &mut HashSet<String>,
) {
    let (project_addr, target_name) = match parse_target_address(target_addr) {
        Some((p, t)) => (p, t),
        None => return,
    };

    let project = match project_map.get(&project_addr) {
        Some(p) => p,
        None => return,
    };

    let target = match project.targets.get(&target_name) {
        Some(t) => t,
        None => return,
    };

    for dep in &target.depends_on {
        if collected.insert(dep.clone()) {
            collect_target_deps(dep, project_map, collected);
        }
    }
}

/// Test-only switch that makes the runner thread of chosen targets panic,
/// to exercise the panic report path. Keyed by address so parallel tests
/// do not interfere.
#[cfg(test)]
mod test_hooks {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static PANIC_ADDRS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

    pub(super) fn panic_on(addr: &str) {
        let set = PANIC_ADDRS.get_or_init(Default::default);
        set.lock().unwrap().insert(addr.to_string());
    }

    pub(super) fn should_panic(addr: &str) -> bool {
        PANIC_ADDRS
            .get()
            .is_some_and(|set| set.lock().unwrap().contains(addr))
    }
}

/// Everything `launch_target` shares across targets for one run.
struct LaunchContext<'a> {
    project_map: &'a HashMap<String, &'a DiscoveredProject>,
    command_overrides: Option<&'a HashMap<String, CommandOverride>>,
    cache_store: Option<&'a ArtifactCache>,
    plugin_registry: &'a PluginRegistry,
    computed_hashes: &'a Arc<Mutex<HashMap<String, String>>>,
    input_hashes: &'a Mutex<HashMap<String, String>>,
    env_snapshot: &'a HashMap<String, String>,
    runtime_inputs: &'a Arc<RuntimeInputCache>,
    resource_mutexes: &'a HashMap<String, Arc<Mutex<()>>>,
    tx: &'a mpsc::Sender<ExecutionResult>,
}

/// One mutex per `exclusive_resources` name used by `targets`.
fn exclusive_resource_locks(
    targets: &HashSet<String>,
    project_map: &HashMap<String, &DiscoveredProject>,
) -> HashMap<String, Arc<Mutex<()>>> {
    let mut resources = HashSet::new();
    for addr in targets {
        if let Some((project_addr, target_name)) = parse_target_address(addr) {
            if let Some(target) = project_map
                .get(&project_addr)
                .and_then(|project| project.targets.get(&target_name))
            {
                resources.extend(target.exclusive_resources.iter().cloned());
            }
        }
    }
    resources
        .into_iter()
        .map(|resource| (resource, Arc::new(Mutex::new(()))))
        .collect()
}

/// Dependency-ready queue over the targets of one run.
///
/// A target becomes ready when every dependency inside the run has
/// completed (passed, failed or been blocked). Ready targets come out ordered
/// by display level, then address, so launch order is deterministic.
struct ReadyQueue<'a> {
    /// Dependencies of each target that have not completed yet.
    unmet: HashMap<String, HashSet<String>>,
    dependents: HashMap<String, Vec<String>>,
    ready: BTreeSet<(usize, String)>,
    level_of: &'a HashMap<String, usize>,
}

impl<'a> ReadyQueue<'a> {
    fn new(
        targets: &HashSet<String>,
        project_map: &HashMap<String, &DiscoveredProject>,
        level_of: &'a HashMap<String, usize>,
    ) -> Self {
        let mut queue = Self {
            unmet: HashMap::new(),
            dependents: HashMap::new(),
            ready: BTreeSet::new(),
            level_of,
        };
        for addr in targets {
            let deps: HashSet<String> = target_dependencies(addr, project_map)
                .into_iter()
                .filter(|dep| dep != addr && targets.contains(dep))
                .collect();
            for dep in &deps {
                queue
                    .dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(addr.clone());
            }
            if deps.is_empty() {
                queue.ready.insert((queue.level(addr), addr.clone()));
            } else {
                queue.unmet.insert(addr.clone(), deps);
            }
        }
        queue
    }

    fn level(&self, addr: &str) -> usize {
        self.level_of.get(addr).copied().unwrap_or(0)
    }

    /// Take every ready target, lowest level and address first.
    fn take_ready(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ready)
            .into_iter()
            .map(|(_, addr)| addr)
            .collect()
    }

    /// Take the targets still waiting once nothing is ready or running.
    /// Only a dependency cycle (or what hangs off one) leaves any.
    fn take_stuck(&mut self) -> Vec<String> {
        let level_of = self.level_of;
        let mut stuck: Vec<(usize, String)> = self
            .unmet
            .drain()
            .map(|(addr, _)| (level_of.get(&addr).copied().unwrap_or(0), addr))
            .collect();
        stuck.sort();
        stuck.into_iter().map(|(_, addr)| addr).collect()
    }

    /// Record that `addr` finished, releasing dependents whose last
    /// dependency it was.
    fn complete(&mut self, addr: &str) {
        for dependent in self.dependents.remove(addr).unwrap_or_default() {
            let Some(unmet) = self.unmet.get_mut(&dependent) else {
                continue;
            };
            unmet.remove(addr);
            if unmet.is_empty() {
                self.unmet.remove(&dependent);
                self.ready.insert((self.level(&dependent), dependent));
            }
        }
    }
}

/// Compute DAG levels for target execution
///
/// Level 0 = targets with no dependencies (in our set)
/// Level N = targets whose dependencies are all in levels 0..N-1
pub fn compute_target_levels(
    targets: &HashSet<String>,
    project_map: &HashMap<String, &DiscoveredProject>,
) -> Vec<Vec<String>> {
    // Build dependency map for targets in our set
    let mut deps_map: HashMap<String, HashSet<String>> = HashMap::new();

    for target_addr in targets {
        let (project_addr, target_name) = match parse_target_address(target_addr) {
            Some((p, t)) => (p, t),
            None => continue,
        };

        let project = match project_map.get(&project_addr) {
            Some(p) => p,
            None => continue,
        };

        let target_deps: HashSet<String> = project
            .targets
            .get(&target_name)
            .map(|t| {
                t.depends_on
                    .iter()
                    .filter(|d| targets.contains(*d))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        deps_map.insert(target_addr.clone(), target_deps);
    }

    // Track which level each target is assigned to
    let mut level_of: HashMap<String, usize> = HashMap::new();
    let mut remaining: HashSet<String> = targets.clone();
    let mut current_level = 0;

    while !remaining.is_empty() {
        let mut this_level = Vec::new();

        for addr in &remaining {
            let deps = deps_map.get(addr).cloned().unwrap_or_default();

            // Check if all deps are assigned to previous levels
            let all_deps_assigned = deps
                .iter()
                .all(|d| level_of.get(d).map(|l| *l < current_level).unwrap_or(false));

            let no_deps = deps.is_empty();

            if all_deps_assigned || no_deps {
                this_level.push(addr.clone());
            }
        }

        // If we made no progress, there's a cycle - just add remaining
        if this_level.is_empty() && !remaining.is_empty() {
            this_level = remaining.iter().cloned().collect();
        }

        // Assign levels and remove from remaining
        for addr in &this_level {
            level_of.insert(addr.clone(), current_level);
            remaining.remove(addr);
        }

        current_level += 1;
    }

    // Build level vectors
    let max_level = level_of.values().max().copied().unwrap_or(0);
    let mut levels: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];

    for (addr, level) in level_of {
        levels[level].push(addr);
    }

    levels
}

/// Plugin cache inputs plus the workspace-root files that plugin names
/// for this project. A root project gets an empty workspace list, so its
/// own `Cargo.lock` is not hashed a second time.
fn cache_inputs_for(
    plugin: &dyn LanguagePlugin,
    project_root: &Path,
    workspace_root: &Path,
    target_name: &str,
) -> crate::plugins::CacheInputs {
    let mut inputs = plugin.cache_inputs(target_name);
    inputs.workspace_files =
        plugin.workspace_cache_inputs(project_root, workspace_root, target_name);
    inputs
}

/// The plugin cache inputs for a target, or `None` when its cache policy
/// leaves it uncacheable.
///
/// Targets with no detected inputs are only cacheable by explicit opt-in;
/// their command, dependencies, configured inputs, and environment still
/// form the cache key.
fn cacheable_inputs(
    target_name: &str,
    project: &DiscoveredProject,
    workspace_root: &Path,
    plugin_registry: &PluginRegistry,
) -> Option<crate::plugins::CacheInputs> {
    let target = project.targets.get(target_name)?;
    let explicitly_enabled = target.cache.as_ref().and_then(|config| config.enabled);
    if explicitly_enabled == Some(false)
        || (explicitly_enabled != Some(true) && !is_default_cacheable_target(target_name))
    {
        return None;
    }
    let plugin = plugin_registry.find_by_name(&project.plugin_name)?;
    let plugin_inputs = cache_inputs_for(plugin, &project.root, workspace_root, target_name);
    if plugin_inputs.source_globs.is_empty()
        && plugin_inputs.config_files.is_empty()
        && plugin_inputs.workspace_files.is_empty()
        && plugin_inputs.env_vars.is_empty()
        && plugin_inputs.path_env_vars.is_empty()
        && explicitly_enabled != Some(true)
    {
        return None;
    }
    Some(plugin_inputs)
}

/// The cache key a fresh run would compute for `target_addr` with its
/// default command, recomputing every dependency's key from current inputs.
///
/// Returns `None` when the target, or any dependency, is not cacheable or
/// its key cannot be computed; execution then caches nothing for it either.
/// `keys` memoizes results by address.
pub fn fresh_cache_key(
    target_addr: &str,
    project_map: &HashMap<String, &DiscoveredProject>,
    workspace_root: &Path,
    plugin_registry: &PluginRegistry,
    env_snapshot: &HashMap<String, String>,
    runtime_inputs: &RuntimeInputCache,
    keys: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    if let Some(key) = keys.get(target_addr) {
        return key.clone();
    }
    // Guards a dependency cycle; resolution rejects cycles before this.
    keys.insert(target_addr.to_string(), None);
    let key = (|| {
        let (project_addr, target_name) = parse_target_address(target_addr)?;
        let project = *project_map.get(&project_addr)?;
        let target = project.targets.get(&target_name)?;
        let plugin_inputs =
            cacheable_inputs(&target_name, project, workspace_root, plugin_registry)?;
        let dep_hashes = target
            .depends_on
            .iter()
            .map(|dep| {
                fresh_cache_key(
                    dep,
                    project_map,
                    workspace_root,
                    plugin_registry,
                    env_snapshot,
                    runtime_inputs,
                    keys,
                )
            })
            .collect::<Option<Vec<_>>>()?;
        let dep_hashes: Vec<&str> = dep_hashes.iter().map(String::as_str).collect();
        CacheHasher::new(&project.root, workspace_root)
            .with_excluded_dirs(nested_project_roots(project, project_map))
            .compute_hash(
                &plugin_inputs,
                target.cache.as_ref(),
                &target.command,
                &dep_hashes,
                env_snapshot,
                runtime_inputs,
            )
            .ok()
    })();
    keys.insert(target_addr.to_string(), key.clone());
    key
}

/// A cache lookup that can be stored, or a hit whose outputs were restored.
enum CacheLookup {
    /// Manifest key matches, outputs were restored, and the log blob was read.
    Hit { manifest: Manifest, log: Vec<u8> },
    /// Cacheable, but nothing reusable is stored. Carries the key to store.
    Miss(String),
}

/// Check if a target is cached.
///
/// `Some(Hit)` when outputs were restored. `Some(Miss)` carries the hash to
/// store after a successful run. `None` when caching cannot be performed.
#[allow(clippy::too_many_arguments)]
fn check_cache(
    target_addr: &str,
    target_name: &str,
    command: &str,
    project: &DiscoveredProject,
    workspace_root: &Path,
    cache_store: &ArtifactCache,
    plugin_registry: &PluginRegistry,
    computed_hashes: &Arc<Mutex<HashMap<String, String>>>,
    env_snapshot: &HashMap<String, String>,
    excluded_dirs: &[PathBuf],
    runtime_inputs: &RuntimeInputCache,
) -> Option<CacheLookup> {
    let target = project.targets.get(target_name)?;
    let user_config: Option<&CacheConfig> = target.cache.as_ref();
    let plugin_inputs = cacheable_inputs(target_name, project, workspace_root, plugin_registry)?;

    // Get dependency hashes from previously computed targets
    let hashes_guard = computed_hashes.lock().ok()?;
    let dep_hashes: Vec<&str> = target
        .depends_on
        .iter()
        .map(|dep| hashes_guard.get(dep).map(String::as_str))
        .collect::<Option<_>>()?;

    // Compute current hash. An unreadable matched input is not cacheable
    // for this run: the target still executes, and nothing is stored.
    let hasher =
        CacheHasher::new(&project.root, workspace_root).with_excluded_dirs(excluded_dirs.to_vec());
    let current_hash = match hasher.compute_hash(
        &plugin_inputs,
        user_config,
        command,
        &dep_hashes,
        env_snapshot,
        runtime_inputs,
    ) {
        Ok(hash) => hash,
        Err(error) => {
            eprintln!("[aster] Warning: Target {target_addr} is not cacheable: {error:#}");
            return None;
        }
    };

    drop(hashes_guard);

    let project_addr = project_of(target_addr).unwrap_or(target_addr);
    match cache_store.lookup(project_addr, target_addr, &current_hash) {
        Ok(Some(manifest)) if manifest.exit_code == 0 => {
            let store = cache_store.local_store();
            // Load the log before restore so a missing blob is a miss, same
            // as a restore failure: the target runs instead of replaying nothing.
            match load_log(store, &manifest) {
                Err(error) => {
                    eprintln!(
                        "[aster] Warning: cache log missing for {target_addr}, running instead: {error:#}"
                    );
                }
                Ok(log) => {
                    if let Err(error) = restore(&project.root, &manifest.outputs, store) {
                        eprintln!(
                            "[aster] Warning: cache restore failed for {target_addr}, running instead: {error:#}"
                        );
                    } else {
                        let mut hashes = computed_hashes.lock().ok()?;
                        hashes.insert(target_addr.to_string(), current_hash);
                        return Some(CacheLookup::Hit {
                            manifest,
                            log: log.unwrap_or_default(),
                        });
                    }
                }
            }
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!(
                "[aster] Warning: cache lookup failed for {target_addr}, running instead: {error:#}"
            );
        }
    }

    Some(CacheLookup::Miss(current_hash))
}

/// Print a cache hit's stored log the way `aster logs` prints a stored target log.
fn print_replayed_log(output: &str) {
    if output.is_empty() {
        return;
    }
    print!("{output}");
    if !output.ends_with('\n') {
        println!();
    }
}

/// Build the result a cache hit reports, including the stored log.
///
/// The log is the `Manifest.log` blob. A missing digest is an empty log,
/// which is what a silent success stored. Callers treat a blob that cannot
/// be read as a miss before calling this.
fn cached_result(
    address: &str,
    manifest: &Manifest,
    log: Vec<u8>,
    input_hash: Option<String>,
) -> ExecutionResult {
    ExecutionResult {
        address: address.to_string(),
        success: true,
        skipped: false,
        cached: true,
        output: String::from_utf8_lossy(&log).into_owned(),
        log,
        duration_ms: u128::from(manifest.duration_ms),
        exit_code: Some(manifest.exit_code),
        tests: None,
        input_hash,
    }
}

/// Read `manifest.log`. `Ok(None)` is a manifest with no log. `Err` means
/// the blob is missing or corrupt, so the hit must not be replayed.
fn load_log(
    store: &crate::cache::LocalStore,
    manifest: &Manifest,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(digest) = &manifest.log else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    store.get_blob(digest, &mut bytes)?;
    Ok(Some(bytes))
}

/// Pack outputs and the log, then store the manifest and project index.
///
/// `Ok(None)` from pack means a declared output is missing or unsafe, so
/// nothing is stored and any previous index entry is removed.
fn store_success(
    cache: &ArtifactCache,
    project_root: &Path,
    address: &str,
    key: &str,
    cache_config: Option<&CacheConfig>,
    result: &ExecutionResult,
) -> anyhow::Result<()> {
    let globs = cache_config
        .and_then(|config| config.outputs.as_deref())
        .unwrap_or(&[]);
    let Some(outputs) = pack(project_root, globs, cache.local_store())? else {
        cache.remove(address)?;
        return Ok(());
    };
    let log = if result.log.is_empty() {
        None
    } else {
        let digest = Digest::of_bytes(&result.log);
        cache
            .local_store()
            .put_blob(&digest, result.log.as_slice())?;
        Some(digest)
    };
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        key: key.to_string(),
        target: address.to_string(),
        exit_code: 0,
        duration_ms: u64::try_from(result.duration_ms).unwrap_or(u64::MAX),
        created_at: Utc::now().to_rfc3339(),
        outputs,
        log,
        aster_version: env!("CARGO_PKG_VERSION").to_string(),
        packs: Vec::new(),
    };
    let project = project_of(address).unwrap_or(address);
    cache.store(project, address, &manifest)
}

/// Hash a target's inputs for test tracking, whatever its cache policy.
///
/// The inputs are the plugin's cache inputs for the target joined with its
/// inputs for `test`, so a custom test target such as `test-ci` still covers
/// the test files, plus the target's configured cache inputs, its command
/// and its dependencies' hashes. A dependency without a hash leaves the
/// target without one: a hash that missed a changed dependency would make a
/// real failure look like a flake.
///
/// A command that runs in another project's directory, through the
/// target's `working_dir` or a one-liner such as
/// `bash -c 'cd ../platform && exec mix test'`, runs that project's tests,
/// so that project's test inputs are hashed too. When the directory is not
/// a known project the target gets no hash.
#[allow(clippy::too_many_arguments)]
fn test_input_hash(
    target_addr: &str,
    target_name: &str,
    command: &str,
    project: &DiscoveredProject,
    workspace_root: &Path,
    project_map: &HashMap<String, &DiscoveredProject>,
    plugin_registry: &PluginRegistry,
    input_hashes: &Mutex<HashMap<String, String>>,
    env_snapshot: &HashMap<String, String>,
    excluded_dirs: &[PathBuf],
    runtime_inputs: &RuntimeInputCache,
) -> Option<String> {
    let target = project.targets.get(target_name)?;
    let elsewhere = tests_run_elsewhere(project, target, command, project_map)?;
    let elsewhere_hash = match elsewhere {
        Some(other) => {
            let other_plugin = plugin_registry.find_by_name(&other.plugin_name)?;
            Some(
                match CacheHasher::new(&other.root, workspace_root)
                    .with_excluded_dirs(nested_project_roots(other, project_map))
                    .compute_hash(
                        &cache_inputs_for(other_plugin, &other.root, workspace_root, "test"),
                        None,
                        "",
                        &[],
                        env_snapshot,
                        runtime_inputs,
                    ) {
                    Ok(hash) => hash,
                    Err(error) => format!("runtime-unavailable:{error:#}"),
                },
            )
        }
        None => None,
    };
    let plugin = plugin_registry.find_by_name(&project.plugin_name)?;
    let mut inputs = cache_inputs_for(plugin, &project.root, workspace_root, target_name);
    let test_inputs = cache_inputs_for(plugin, &project.root, workspace_root, "test");
    for (all, more) in [
        (&mut inputs.source_globs, test_inputs.source_globs),
        (&mut inputs.config_files, test_inputs.config_files),
        (&mut inputs.workspace_files, test_inputs.workspace_files),
        (&mut inputs.env_vars, test_inputs.env_vars),
        (&mut inputs.path_env_vars, test_inputs.path_env_vars),
        (&mut inputs.runtime_commands, test_inputs.runtime_commands),
    ] {
        for item in more {
            if !all.contains(&item) {
                all.push(item);
            }
        }
    }
    let hash = {
        let hashes = input_hashes.lock().ok()?;
        let mut dependencies: Vec<&str> = target
            .depends_on
            .iter()
            .map(|dependency| hashes.get(dependency).map(String::as_str))
            .collect::<Option<_>>()?;
        dependencies.extend(elsewhere_hash.as_deref());
        match CacheHasher::new(&project.root, workspace_root)
            .with_excluded_dirs(excluded_dirs.to_vec())
            .compute_hash(
                &inputs,
                target.cache.as_ref(),
                command,
                &dependencies,
                env_snapshot,
                runtime_inputs,
            ) {
            Ok(hash) => hash,
            // A missing toolchain must not drop test tracking. Cache hits
            // already refuse the target; the test hash records the failure
            // instead of pretending the target has no inputs.
            Err(error) => format!("runtime-unavailable:{error:#}"),
        }
    };
    input_hashes
        .lock()
        .ok()?
        .insert(target_addr.to_string(), hash.clone());
    Some(hash)
}

/// The project whose tests a target's command runs when that is not the
/// target's own project. `Some(None)`: the command runs in its own project.
/// `None`: it runs in a directory that is no known project.
fn tests_run_elsewhere<'p>(
    project: &DiscoveredProject,
    target: &crate::plugins::Target,
    command: &str,
    project_map: &HashMap<String, &'p DiscoveredProject>,
) -> Option<Option<&'p DiscoveredProject>> {
    let mut dir = target
        .working_dir
        .clone()
        .unwrap_or_else(|| project.root.clone());
    if let Some(cd) = crate::plugins::related_tests::Delegated::parse(command).and_then(|d| d.dir) {
        dir = dir.join(cd);
    }
    let canonical = |path: &Path| {
        std::fs::canonicalize(path).unwrap_or_else(|_| {
            // Resolve `..` by hand for a path that does not exist.
            let mut normal = PathBuf::new();
            for component in path.components() {
                match component {
                    std::path::Component::ParentDir => {
                        normal.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => normal.push(other),
                }
            }
            normal
        })
    };
    let dir = canonical(&dir);
    if dir == canonical(&project.root) {
        return Some(None);
    }
    project_map
        .values()
        .find(|other| canonical(&other.root) == dir)
        .map(|other| Some(*other))
}

fn is_default_cacheable_target(target_name: &str) -> bool {
    matches!(
        target_name,
        "deps" | "build" | "test" | "lint" | "format" | "typecheck" | "check"
    )
}

/// Project roots that live under `project` and must not enter its cache key.
///
/// A nested Aster project is hashed on its own. Descending into it from the
/// parent would put the child's files in both keys, so a child-only change
/// would miss the parent's cache. The project's own root is not excluded.
/// Roots are taken from the executor's project map, which under `--no-deps`
/// contains only the selected projects; an unselected nested project is then
/// not excluded, which can only cause a cache miss.
fn nested_project_roots(
    project: &DiscoveredProject,
    project_map: &HashMap<String, &DiscoveredProject>,
) -> Vec<PathBuf> {
    let parent = project.root.as_path();
    let mut roots: Vec<PathBuf> = project_map
        .values()
        .filter(|other| other.root != parent && other.root.starts_with(parent))
        .map(|other| other.root.clone())
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// One command of a target as it is run.
struct Invocation {
    /// The command as written, shown in logs.
    shown: String,
    /// The command actually run: `shown`, or its test-capture rewrite.
    command: String,
    /// Environment added for this command.
    env: Vec<(String, String)>,
    /// Keep the raw console output: it is the command's test report.
    keep_output: bool,
    /// How to render the raw console output for the log.
    rewrite: Option<OutputRewrite>,
}

impl Invocation {
    fn plain(command: &str) -> Self {
        Self {
            shown: command.to_string(),
            command: command.to_string(),
            env: Vec::new(),
            keep_output: false,
            rewrite: None,
        }
    }

    fn planned(command: &str, plan: &ReportPlan) -> Self {
        Self {
            shown: command.to_string(),
            command: plan.command.clone(),
            env: plan.env.clone(),
            keep_output: plan.files == ReportFiles::Stdout || plan.format.reads_console(),
            rewrite: plan.rewrite,
        }
    }

    /// Run the command. Returns its result, with the console output
    /// rendered for the log, and the raw output when it is kept.
    fn run(
        &self,
        address: &str,
        working_dir: &Path,
        null_stdin: bool,
        log_cap: usize,
    ) -> (ExecutionResult, Option<String>) {
        let mut result = run_command(
            address,
            &self.command,
            &self.env,
            working_dir,
            null_stdin,
            log_cap,
        );
        let raw = self.rewrite.map(|rewrite| {
            let rendered = test_results::rewrite_output(rewrite, &result.output);
            let previous = std::mem::replace(&mut result.output, rendered);
            result.log = result.output.as_bytes().to_vec();
            previous
        });
        let raw = if self.keep_output {
            raw.or_else(|| Some(result.output.clone()))
        } else {
            None
        };
        (result, raw)
    }
}

/// Ask the plugin how to capture test results from each command. Each
/// planned command gets its own empty report directory.
fn plan_captures(
    plugin: &dyn LanguagePlugin,
    declared: Option<&crate::config::ReportConfig>,
    project_dir: &Path,
    working_dir: &Path,
    target_name: &str,
    commands: &[String],
) -> Vec<Option<(ReportPlan, PathBuf)>> {
    let last = commands.len().saturating_sub(1);
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            // Declared report files at fixed paths in the project are
            // overwritten by each command of a split target, so only the
            // last command's are read; the earlier commands go unread and
            // the results are partial.
            if declared.is_some_and(|report| !report.files.is_empty()) && index != last {
                return None;
            }
            let report_dir = match test_results::new_report_dir() {
                Ok(dir) => dir,
                Err(error) => {
                    eprintln!("[aster] Warning: test results will not be captured: {error:#}");
                    return None;
                }
            };
            let context = ReportContext {
                project_dir,
                working_dir,
                target_name,
                command,
                report_dir: &report_dir,
            };
            // A report the target declares wins: it says what the command
            // really runs, which recognising the command text can only guess.
            let plan = match declared {
                Some(report) => Some(crate::plugins::test_reports::declared(report, &context)),
                None => plugin
                    .test_report_plan(&context)
                    .or_else(|| crate::plugins::test_reports::recognise(&context)),
            };
            match plan {
                Some(plan) => Some((plan, report_dir)),
                None => {
                    let _ = std::fs::remove_dir_all(&report_dir);
                    None
                }
            }
        })
        .collect()
}

/// Read the reports of the commands that ran and remove every report
/// directory. `raw_outputs` has one entry per command that ran: its raw
/// console output when that is its report. `None` when no command was
/// planned for capture.
///
/// The result is [`Confidence::Complete`] only when every command ran, was
/// planned, its plan was complete and its report was read in full.
fn read_test_report(
    address: &str,
    captures: &[Option<(ReportPlan, PathBuf)>],
    raw_outputs: &[Option<String>],
    started: SystemTime,
) -> Option<TestReport> {
    if captures.iter().all(Option::is_none) {
        return None;
    }
    let mut complete = raw_outputs.len() == captures.len();
    let mut cases = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for (capture, raw) in captures.iter().zip(raw_outputs) {
        let Some((plan, _)) = capture else {
            complete = false;
            continue;
        };
        match test_results::collect(plan, started, raw.as_deref()) {
            Ok(found) => {
                complete &= found.complete && plan.confidence == Confidence::Complete;
                cases.extend(found.cases);
                let name = if plan.declared {
                    plan.format.declared_capture_name()
                } else {
                    plan.format.capture_name()
                };
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            Err(error) => {
                eprintln!("[aster] Warning: test results for {address} are incomplete: {error:#}");
                complete = false;
            }
        }
    }
    for (_, dir) in captures.iter().flatten() {
        let _ = std::fs::remove_dir_all(dir);
    }
    Some(TestReport {
        project: None,
        capture: names.join("+"),
        confidence: if complete {
            Confidence::Complete
        } else {
            Confidence::Partial
        },
        cases,
        quarantined: Vec::new(),
    })
}

/// Run commands in order, stopping at the first failure. Returns the
/// combined result and, for each command that ran, its raw console output
/// when the command keeps it.
///
/// A target has one command; several come only from a
/// [`CommandOverride::Run`] that splits the target into narrower invocations.
fn run_invocations(
    address: &str,
    invocations: &[Invocation],
    working_dir: &Path,
    null_stdin: bool,
    log_cap: usize,
) -> (ExecutionResult, Vec<Option<String>>) {
    if let [invocation] = invocations {
        let (result, raw) = invocation.run(address, working_dir, null_stdin, log_cap);
        return (result, vec![raw]);
    }
    let mut combined = ExecutionResult {
        address: address.to_string(),
        success: true,
        skipped: false,
        cached: false,
        output: String::new(),
        log: Vec::new(),
        duration_ms: 0,
        exit_code: None,
        tests: None,
        input_hash: None,
    };
    let mut raw_outputs = Vec::new();
    for invocation in invocations {
        if shutdown_requested() {
            break;
        }
        let (result, raw) = invocation.run(address, working_dir, null_stdin, log_cap);
        raw_outputs.push(raw);
        if !combined.log.is_empty() && !combined.log.ends_with(b"\n") {
            combined.log.push(b'\n');
        }
        combined
            .log
            .extend_from_slice(format!("$ {}\n", invocation.shown).as_bytes());
        combined.log.extend_from_slice(&result.log);
        combined.output = String::from_utf8_lossy(&combined.log).into_owned();
        combined.duration_ms += result.duration_ms;
        combined.exit_code = result.exit_code;
        if !result.success {
            combined.success = false;
            break;
        }
    }
    (combined, raw_outputs)
}

/// Shared, capped buffer of one command's stdout and stderr.
///
/// Readers append chunks under the mutex in arrival order. Once `cap` bytes
/// are kept, later bytes are counted and dropped. `finish` appends the
/// truncation marker.
struct CappedLog {
    buf: Mutex<Vec<u8>>,
    cap: usize,
    /// Bytes received and not retained. A short read that fills the cap still
    /// counts its kept bytes as received, so a later drop is `received - kept`.
    received: Mutex<u64>,
}

impl CappedLog {
    fn new(cap: usize) -> Self {
        Self {
            buf: Mutex::new(Vec::new()),
            cap,
            received: Mutex::new(0),
        }
    }

    fn append(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let mut buf = self.buf.lock().unwrap();
        let mut received = self.received.lock().unwrap();
        *received += chunk.len() as u64;
        let room = self.cap.saturating_sub(buf.len());
        let keep = room.min(chunk.len());
        buf.extend_from_slice(&chunk[..keep]);
    }

    fn finish(self) -> Vec<u8> {
        let mut buf = self.buf.into_inner().unwrap();
        let received = self.received.into_inner().unwrap();
        let omitted = received.saturating_sub(buf.len() as u64);
        if omitted > 0 {
            let marker = format!("\n[aster] log truncated: {omitted} bytes omitted\n");
            buf.extend_from_slice(marker.as_bytes());
        }
        buf
    }
}

/// Read a pipe to EOF, appending each chunk to `log` as it arrives.
fn spawn_log_reader<R>(mut reader: R, log: Arc<CappedLog>) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => log.append(&chunk[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

/// Exit code of a finished child.
///
/// A normal exit is the status code. A unix signal death is `128 + signal`,
/// matching the shell. `None` only when the platform reports neither.
fn exit_code_of(status: std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(128 + signal);
        }
    }
    None
}

/// Run a command in a directory and capture its combined log.
fn run_command(
    address: &str,
    command: &str,
    extra_env: &[(String, String)],
    working_dir: &Path,
    null_stdin: bool,
    log_cap: usize,
) -> ExecutionResult {
    let start = Instant::now();

    let failed = |output: String| ExecutionResult {
        address: address.to_string(),
        success: false,
        skipped: false,
        cached: false,
        log: output.as_bytes().to_vec(),
        output,
        duration_ms: 0,
        exit_code: None,
        tests: None,
        input_hash: None,
    };

    let parsed = match parse_command(command) {
        Ok(parsed) => parsed,
        Err(error) => return failed(error.to_string()),
    };

    if parsed.program.is_empty() {
        return failed("Empty command".to_string());
    }

    let mut cmd = Command::new(&parsed.program);
    cmd.args(&parsed.args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if null_stdin {
        // Closed stdin makes isatty(0) false even after setsid(): the child
        // still inherits a TTY *fd* unless we replace it. Tools that prompt
        // when stdin is a TTY then hang behind the progress UI.
        cmd.stdin(Stdio::null());
    }

    for (name, value) in parsed.env {
        cmd.env(name, value);
    }
    for (name, value) in extra_env {
        cmd.env(name, value);
    }

    #[cfg(unix)]
    isolate_streaming_session(&mut cmd);
    #[cfg(windows)]
    prepare_windows_child(&mut cmd);

    #[cfg(any(unix, windows))]
    register_supervised_child();
    let spawned = cmd.spawn().inspect_err(|_| {
        #[cfg(any(unix, windows))]
        unregister_supervised_child();
    });

    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let output = format!("Failed to execute command: {error}");
            return ExecutionResult {
                address: address.to_string(),
                success: false,
                skipped: false,
                cached: false,
                output: output.clone(),
                log: output.into_bytes(),
                duration_ms: start.elapsed().as_millis(),
                exit_code: None,
                tests: None,
                input_hash: None,
            };
        }
    };

    #[cfg(unix)]
    let monitor = ChildSignalMonitor::new(child.id());
    #[cfg(windows)]
    let monitor = match WindowsChildSignalMonitor::new(&mut child) {
        Ok(monitor) => monitor,
        Err(error) => {
            // `new` may already have resumed the suspended child. Reap it
            // before dropping supervision, or it keeps running outside the
            // shutdown watcher. Drop neither kills nor waits.
            let _ = child.kill();
            let _ = child.wait();
            unregister_supervised_child();
            let output = format!("Failed to execute command: {error}");
            return ExecutionResult {
                address: address.to_string(),
                success: false,
                skipped: false,
                cached: false,
                output: output.clone(),
                log: output.into_bytes(),
                duration_ms: start.elapsed().as_millis(),
                exit_code: None,
                tests: None,
                input_hash: None,
            };
        }
    };

    let log = Arc::new(CappedLog::new(log_cap));
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = spawn_log_reader(stdout, Arc::clone(&log));
    let stderr_reader = spawn_log_reader(stderr, Arc::clone(&log));

    let status = child.wait();
    // Join readers after the child exits so a full pipe cannot deadlock the wait.
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();

    #[cfg(any(unix, windows))]
    monitor.finish();

    let duration_ms = start.elapsed().as_millis();
    match status {
        Ok(status) => {
            let log = Arc::into_inner(log)
                .expect("log readers have joined")
                .finish();
            let output = String::from_utf8_lossy(&log).into_owned();
            ExecutionResult {
                address: address.to_string(),
                success: status.success(),
                skipped: false,
                cached: false,
                output,
                log,
                duration_ms,
                exit_code: exit_code_of(status),
                tests: None,
                input_hash: None,
            }
        }
        Err(error) => {
            let output = format!("Failed to execute command: {error}");
            ExecutionResult {
                address: address.to_string(),
                success: false,
                skipped: false,
                cached: false,
                output: output.clone(),
                log: output.into_bytes(),
                duration_ms,
                exit_code: None,
                tests: None,
                input_hash: None,
            }
        }
    }
}

#[cfg(windows)]
fn prepare_windows_child(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
}

#[cfg(windows)]
struct WindowsChildSignalMonitor {
    completed: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    termination_handle: windows_sys::Win32::Foundation::HANDLE,
    job_backed: bool,
    registered: bool,
}

#[cfg(windows)]
impl WindowsChildSignalMonitor {
    fn new(child: &mut std::process::Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            let assigned = AssignProcessToJobObject(job, child.as_raw_handle() as _) != 0;
            let assignment_error = (!assigned).then(std::io::Error::last_os_error);
            let (termination_handle, job_backed) = if assigned {
                (job, true)
            } else if assignment_error
                .as_ref()
                .and_then(std::io::Error::raw_os_error)
                == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32)
            {
                windows_sys::Win32::Foundation::CloseHandle(job);
                (std::ptr::null_mut(), false)
            } else {
                let error = assignment_error.unwrap_or_else(std::io::Error::last_os_error);
                windows_sys::Win32::Foundation::CloseHandle(job);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            };
            if let Err(error) = resume_windows_process(child.id()) {
                windows_sys::Win32::Foundation::CloseHandle(termination_handle);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }

            let completed = Arc::new(AtomicBool::new(false));
            let completed_for_thread = completed.clone();
            let termination_value = termination_handle as usize;
            let root_process_id = child.id();
            let handle = thread::spawn(move || {
                while !completed_for_thread.load(Ordering::SeqCst) {
                    if shutdown_requested() {
                        let handle = termination_value as windows_sys::Win32::Foundation::HANDLE;
                        if job_backed {
                            windows_sys::Win32::System::JobObjects::TerminateJobObject(handle, 1);
                        } else {
                            crate::windows_process::terminate_process_tree(root_process_id);
                        }
                        return;
                    }
                    thread::park_timeout(Duration::from_millis(20));
                }
            });
            Ok(Self {
                completed,
                handle: Some(handle),
                termination_handle,
                job_backed,
                registered: true,
            })
        }
    }

    fn finish(mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        self.completed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
        if !self.termination_handle.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.termination_handle);
            }
            self.termination_handle = std::ptr::null_mut();
        }
        if self.registered {
            unregister_supervised_child();
            self.registered = false;
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsChildSignalMonitor {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(windows)]
fn resume_windows_process(process_id: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut has_entry = Thread32First(snapshot, &mut entry) != 0;
        while has_entry {
            if entry.th32OwnerProcessID == process_id {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(error);
                }
                let result = ResumeThread(thread);
                let error = (result == u32::MAX).then(std::io::Error::last_os_error);
                CloseHandle(thread);
                CloseHandle(snapshot);
                return error.map_or(Ok(()), Err);
            }
            has_entry = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "suspended process had no discoverable primary thread",
    ))
}

#[cfg(unix)]
fn isolate_streaming_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                let _ = libc::setpgid(0, 0);
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
struct ChildSignalMonitor {
    completed: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

#[cfg(unix)]
impl ChildSignalMonitor {
    fn new(process_id: u32) -> Self {
        let process_group = process_id as i32;
        let completed = Arc::new(AtomicBool::new(false));
        let completed_for_thread = Arc::clone(&completed);
        let handle = thread::spawn(move || {
            while !completed_for_thread.load(Ordering::SeqCst) {
                if shutdown_requested() {
                    unsafe {
                        libc::kill(-process_group, libc::SIGTERM);
                    }
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while Instant::now() < deadline {
                        if completed_for_thread.load(Ordering::SeqCst) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    unsafe {
                        libc::kill(-process_group, libc::SIGKILL);
                    }
                    return;
                }
                thread::park_timeout(Duration::from_millis(20));
            }
        });

        Self { completed, handle }
    }

    fn finish(self) {
        self.completed.store(true, Ordering::SeqCst);
        self.handle.thread().unpark();
        let _ = self.handle.join();
        unregister_supervised_child();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{ProjectMetadata, Target};
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// Helper to create a Target with empty capabilities
    fn target(command: &str, depends_on: Vec<&str>) -> Target {
        Target {
            command: command.to_string(),
            depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
            exclusive_resources: vec![],
            retry_setup: None,
            report: None,
        }
    }

    fn make_project_with_targets(
        relative_path: &str,
        targets: HashMap<String, Target>,
    ) -> DiscoveredProject {
        DiscoveredProject {
            root: PathBuf::from(format!("/workspace/{relative_path}")),
            config_path: PathBuf::from(format!("/workspace/{relative_path}/package.json")),
            metadata: ProjectMetadata {
                name: relative_path.to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: vec![],
            targets,
            plugin_name: "nodejs".to_string(),
            languages: vec!["nodejs".to_string()],
            build_system: None,
            relative_path: PathBuf::from(relative_path),
        }
    }

    #[test]
    fn tests_run_elsewhere_follows_cd_and_working_dir() {
        // Paths that do not exist compare as written.
        let shard = make_project_with_targets("services/shard", HashMap::new());
        let platform = make_project_with_targets("services/platform", HashMap::new());
        let map: HashMap<String, &DiscoveredProject> = [
            ("//services/shard".to_string(), &shard),
            ("//services/platform".to_string(), &platform),
        ]
        .into_iter()
        .collect();
        let plain = target("mix test", vec![]);

        let own = tests_run_elsewhere(&shard, &plain, "mix test", &map);
        assert!(matches!(own, Some(None)));

        let cd = "bash -c 'cd ../platform && MIX_TEST_PARTITION=1 exec mix test'";
        let other = tests_run_elsewhere(&shard, &plain, cd, &map)
            .unwrap()
            .unwrap();
        assert_eq!(other.root, platform.root);

        let mut moved = plain.clone();
        moved.working_dir = Some(platform.root.clone());
        let other = tests_run_elsewhere(&shard, &moved, "mix test", &map)
            .unwrap()
            .unwrap();
        assert_eq!(other.root, platform.root);

        // A directory that is no project leaves the target without a hash.
        let unknown = "bash -c 'cd ../somewhere && exec mix test'";
        assert!(tests_run_elsewhere(&shard, &plain, unknown, &map).is_none());
    }

    #[test]
    fn test_parse_target_address() {
        let result = parse_target_address("//apps/web:test");
        assert_eq!(result, Some(("//apps/web".to_string(), "test".to_string())));

        let result = parse_target_address("//a:deps");
        assert_eq!(result, Some(("//a".to_string(), "deps".to_string())));

        let result = parse_target_address("invalid");
        assert_eq!(result, None);
    }

    #[test]
    fn test_compute_target_levels_single() {
        let mut targets = HashMap::new();
        targets.insert("test".to_string(), target("npm test", vec![]));
        let project = make_project_with_targets("a", targets);
        let projects = [project];

        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let mut targets_to_run = HashSet::new();
        targets_to_run.insert("//a:test".to_string());

        let levels = compute_target_levels(&targets_to_run, &project_map);

        assert_eq!(levels.len(), 1);
        assert!(levels[0].contains(&"//a:test".to_string()));
    }

    #[test]
    fn test_compute_target_levels_with_deps_target() {
        // test depends on deps in same project
        let mut targets = HashMap::new();
        targets.insert("deps".to_string(), target("npm install", vec![]));
        targets.insert("test".to_string(), target("npm test", vec!["//a:deps"]));
        let project = make_project_with_targets("a", targets);
        let projects = [project];

        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let mut targets_to_run = HashSet::new();
        targets_to_run.insert("//a:test".to_string());
        targets_to_run.insert("//a:deps".to_string());

        let levels = compute_target_levels(&targets_to_run, &project_map);

        assert_eq!(levels.len(), 2);
        assert!(levels[0].contains(&"//a:deps".to_string()));
        assert!(levels[1].contains(&"//a:test".to_string()));
    }

    #[test]
    fn test_compute_target_levels_cross_project() {
        // //b:test depends on //a:build
        let mut targets_a = HashMap::new();
        targets_a.insert("build".to_string(), target("npm run build", vec![]));

        let mut targets_b = HashMap::new();
        targets_b.insert("test".to_string(), target("npm test", vec!["//a:build"]));

        let project_a = make_project_with_targets("a", targets_a);
        let project_b = make_project_with_targets("b", targets_b);
        let projects = [project_a, project_b];

        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let mut targets_to_run = HashSet::new();
        targets_to_run.insert("//a:build".to_string());
        targets_to_run.insert("//b:test".to_string());

        let levels = compute_target_levels(&targets_to_run, &project_map);

        assert_eq!(levels.len(), 2);
        assert!(levels[0].contains(&"//a:build".to_string()));
        assert!(levels[1].contains(&"//b:test".to_string()));
    }

    #[test]
    fn test_collect_target_deps() {
        let mut targets = HashMap::new();
        targets.insert("deps".to_string(), target("npm install", vec![]));
        targets.insert(
            "build".to_string(),
            target("npm run build", vec!["//a:deps"]),
        );
        targets.insert("test".to_string(), target("npm test", vec!["//a:build"]));
        let project = make_project_with_targets("a", targets);
        let projects = [project];

        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let mut collected = HashSet::new();
        collect_target_deps("//a:test", &project_map, &mut collected);

        // Should collect build and deps (transitively)
        assert!(collected.contains("//a:build"));
        assert!(collected.contains("//a:deps"));
    }

    #[test]
    fn test_compute_target_levels_diamond() {
        // deps -> build and lint -> test (test depends on both build and lint)
        let mut targets = HashMap::new();
        targets.insert("deps".to_string(), target("npm install", vec![]));
        targets.insert(
            "build".to_string(),
            target("npm run build", vec!["//a:deps"]),
        );
        targets.insert("lint".to_string(), target("npm run lint", vec!["//a:deps"]));
        targets.insert(
            "test".to_string(),
            target("npm test", vec!["//a:build", "//a:lint"]),
        );
        let project = make_project_with_targets("a", targets);
        let projects = [project];

        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let mut targets_to_run = HashSet::new();
        targets_to_run.insert("//a:deps".to_string());
        targets_to_run.insert("//a:build".to_string());
        targets_to_run.insert("//a:lint".to_string());
        targets_to_run.insert("//a:test".to_string());

        let levels = compute_target_levels(&targets_to_run, &project_map);

        assert_eq!(levels.len(), 3);
        assert!(levels[0].contains(&"//a:deps".to_string()));
        assert!(levels[1].contains(&"//a:build".to_string()));
        assert!(levels[1].contains(&"//a:lint".to_string()));
        assert!(levels[2].contains(&"//a:test".to_string()));
    }

    #[test]
    fn failed_prerequisite_blocks_dependent_command() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("dependent-ran");
        let mut targets = HashMap::new();
        targets.insert("deps".to_string(), target("sh -c 'exit 7'", vec![]));
        targets.insert(
            "build".to_string(),
            target(
                &format!(
                    "touch {}",
                    crate::executor::quote_command_argument(&marker.to_string_lossy())
                ),
                vec!["//a:deps"],
            ),
        );

        let mut project = make_project_with_targets("a", targets);
        project.root = tmp.path().to_path_buf();
        let projects = [project];
        let refs = projects.iter().collect::<Vec<_>>();
        let requested = HashSet::from(["//a:build".to_string()]);
        let executor = Executor::with_all_options(
            tmp.path(),
            crate::cli::output::OutputMode::Json,
            false,
            false,
        );

        let results = executor.execute_targets(&requested, &refs, false);
        let dependency = results
            .iter()
            .find(|result| result.address == "//a:deps")
            .unwrap();
        let dependent = results
            .iter()
            .find(|result| result.address == "//a:build")
            .unwrap();

        assert!(!dependency.success);
        assert!(!dependent.success);
        assert!(dependent.output.contains("Blocked"));
        assert!(!marker.exists(), "blocked dependent command was executed");
    }

    #[test]
    fn custom_targets_only_write_cache_entries_when_explicitly_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let mut targets = HashMap::new();
        targets.insert("deploy".to_string(), target("true", vec![]));
        let mut project = make_project_with_targets("a", targets);
        project.root = tmp.path().to_path_buf();
        project.plugin_name = "nodejs".to_string();
        let requested = HashSet::from(["//a:deploy".to_string()]);
        let executor = Executor::with_output_mode(tmp.path(), crate::cli::output::OutputMode::Json);

        let projects = [&project];
        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        let store = ArtifactCache::new(crate::cache::resolve_cache_dir(tmp.path()), tmp.path());
        assert!(store
            .lookup("//a", "//a:deploy", &"aa".repeat(32))
            .unwrap()
            .is_none());
        store
            .store(
                "//a",
                "//a:deploy",
                &Manifest {
                    schema: MANIFEST_SCHEMA_VERSION,
                    key: "aa".repeat(32),
                    target: "//a:deploy".to_string(),
                    exit_code: 0,
                    duration_ms: 1,
                    created_at: Utc::now().to_rfc3339(),
                    outputs: Vec::new(),
                    log: None,
                    aster_version: "0.21.0".to_string(),
                    packs: Vec::new(),
                },
            )
            .unwrap();
        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        assert!(store
            .lookup("//a", "//a:deploy", &"aa".repeat(32))
            .unwrap()
            .is_none());

        project.targets.get_mut("deploy").unwrap().cache = Some(CacheConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let projects = [&project];
        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        assert!(!store.list_project_entries("//a").unwrap().is_empty());
    }

    #[test]
    fn cache_is_disabled_when_a_dependency_has_no_hash() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let mut targets = HashMap::new();
        targets.insert("build".to_string(), target("true", vec!["//a:prepare"]));
        targets.insert("prepare".to_string(), target("true", vec![]));
        let mut project = make_project_with_targets("a", targets);
        project.root = tmp.path().to_path_buf();
        project.plugin_name = "nodejs".to_string();

        let result = check_cache(
            "//a:build",
            "build",
            "true",
            &project,
            tmp.path(),
            &ArtifactCache::new(crate::cache::resolve_cache_dir(tmp.path()), tmp.path()),
            &PluginRegistry::with_all_plugins(),
            &Arc::new(Mutex::new(HashMap::new())),
            &HashMap::new(),
            &[],
            &RuntimeInputCache::new(tmp.path()),
        );

        assert!(
            result.is_none(),
            "a dependency omitted from the cache key must disable caching"
        );
    }

    #[test]
    fn auto_prune_after_run_drops_old_manifests_over_cap() {
        use crate::cache::{
            Digest, LocalStore, Manifest, OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION,
        };

        let tmp = tempfile::tempdir().unwrap();
        let store = LocalStore::new(crate::cache::resolve_cache_dir(tmp.path()));
        let old_bytes: Vec<u8> = (0..8 * 1024).map(|index| (index * 17) as u8).collect();
        let old = Digest::of_bytes(&old_bytes);
        let new = Digest::of_bytes(b"new-auto-prune-blob");
        store.put_blob(&old, old_bytes.as_slice()).unwrap();
        store.put_blob(&new, &b"new-auto-prune-blob"[..]).unwrap();
        let old_on_disk = std::fs::metadata(store.blob_path(&old).unwrap())
            .unwrap()
            .len();
        let new_on_disk = std::fs::metadata(store.blob_path(&new).unwrap())
            .unwrap()
            .len();
        assert!(
            old_on_disk > new_on_disk,
            "old blob must be larger on disk than the survivor"
        );
        std::fs::write(
            tmp.path().join("aster.toml"),
            format!("[cache]\nmax_size = \"{new_on_disk}\"\nmax_age = \"0\"\n"),
        )
        .unwrap();

        let old_key = "aa3333333333333333";
        let new_key = "bb4444444444444444";
        let manifest = |key: &str, digest: &Digest| Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: key.into(),
            target: "//a:build".into(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-13T00:00:00Z".into(),
            outputs: vec![OutputEntry {
                path: "dist/out.bin".into(),
                kind: OutputKind::File,
                digest: Some(digest.clone()),
                mode: 0o644,
                target: None,
            }],
            log: None,
            aster_version: "0.17.2".into(),
            packs: Vec::new(),
        };
        store.put_manifest(&manifest(old_key, &old)).unwrap();
        store.put_manifest(&manifest(new_key, &new)).unwrap();
        set_manifest_mtime(
            &store.manifest_path(old_key).unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        );
        set_manifest_mtime(
            &store.manifest_path(new_key).unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(20),
        );
        // Past prune's grace window for unreferenced blobs.
        set_manifest_mtime(
            &store.blob_path(&old).unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        );
        assert!(store.has_blob(&old));

        auto_prune_cache(tmp.path(), crate::cli::output::OutputMode::Quiet);

        assert!(store.get_manifest(old_key).unwrap().is_none());
        assert!(store.get_manifest(new_key).unwrap().is_some());
        assert!(!store.has_blob(&old));
        assert!(store.has_blob(&new));
    }

    fn set_manifest_mtime(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    #[test]
    fn nested_project_roots_lists_only_projects_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_root = tmp.path().join("app");
        let child_root = parent_root.join("packages/ui");
        let sibling_root = tmp.path().join("other");
        std::fs::create_dir_all(&child_root).unwrap();
        std::fs::create_dir_all(&sibling_root).unwrap();

        let mut parent = make_project_with_targets("app", HashMap::new());
        parent.root = parent_root.clone();
        parent.relative_path = PathBuf::from("app");
        let mut child = make_project_with_targets("ui", HashMap::new());
        child.root = child_root.clone();
        child.relative_path = PathBuf::from("app/packages/ui");
        let mut sibling = make_project_with_targets("other", HashMap::new());
        sibling.root = sibling_root;
        sibling.relative_path = PathBuf::from("other");

        let projects = [&parent, &child, &sibling];
        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|project| (format!("//{}", project.relative_path.display()), *project))
            .collect();

        assert_eq!(
            nested_project_roots(&parent, &project_map),
            vec![child_root]
        );
        assert!(nested_project_roots(&child, &project_map).is_empty());
        assert!(nested_project_roots(&sibling, &project_map).is_empty());
    }

    #[test]
    fn target_that_changes_its_inputs_is_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/input.js"), "before").unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let mut targets = HashMap::new();
        targets.insert(
            "build".to_string(),
            target("sh -c 'printf after > src/input.js'", vec![]),
        );
        let mut project = make_project_with_targets("a", targets);
        project.root = tmp.path().to_path_buf();
        project.plugin_name = "nodejs".to_string();
        let projects = [&project];
        let requested = HashSet::from(["//a:build".to_string()]);
        let executor = Executor::with_output_mode(tmp.path(), crate::cli::output::OutputMode::Json);
        let store = ArtifactCache::new(crate::cache::resolve_cache_dir(tmp.path()), tmp.path());
        store
            .store(
                "//a",
                "//a:build",
                &Manifest {
                    schema: MANIFEST_SCHEMA_VERSION,
                    key: "bb".repeat(32),
                    target: "//a:build".to_string(),
                    exit_code: 0,
                    duration_ms: 1,
                    created_at: Utc::now().to_rfc3339(),
                    outputs: Vec::new(),
                    log: None,
                    aster_version: "0.21.0".to_string(),
                    packs: Vec::new(),
                },
            )
            .unwrap();

        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        assert!(store
            .lookup("//a", "//a:build", &"bb".repeat(32))
            .unwrap()
            .is_none());
    }

    #[test]
    fn failed_execution_removes_previous_cache_entry() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/input.js"), "input").unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let mut targets = HashMap::new();
        targets.insert(
            "build".to_string(),
            target("sh -c 'printf broken > output; exit 1'", vec![]),
        );
        let mut project = make_project_with_targets("a", targets);
        project.root = tmp.path().to_path_buf();
        project.plugin_name = "nodejs".to_string();
        let store = ArtifactCache::new(crate::cache::resolve_cache_dir(tmp.path()), tmp.path());
        store
            .store(
                "//a",
                "//a:build",
                &Manifest {
                    schema: MANIFEST_SCHEMA_VERSION,
                    key: "bb".repeat(32),
                    target: "//a:build".to_string(),
                    exit_code: 0,
                    duration_ms: 1,
                    created_at: Utc::now().to_rfc3339(),
                    outputs: Vec::new(),
                    log: None,
                    aster_version: "0.21.0".to_string(),
                    packs: Vec::new(),
                },
            )
            .unwrap();

        let projects = [&project];
        let requested = HashSet::from(["//a:build".to_string()]);
        let results = Executor::with_output_mode(tmp.path(), crate::cli::output::OutputMode::Json)
            .execute_targets(&requested, &projects, false);

        assert!(results.iter().any(|result| !result.success));
        assert!(store
            .lookup("//a", "//a:build", &"bb".repeat(32))
            .unwrap()
            .is_none());
    }

    /// Helper to create a Target with exclusive_resources
    fn target_with_resources(command: &str, depends_on: Vec<&str>, resources: Vec<&str>) -> Target {
        Target {
            command: command.to_string(),
            depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
            exclusive_resources: resources.into_iter().map(|s| s.to_string()).collect(),
            retry_setup: None,
            report: None,
        }
    }

    /// Run `targets` of one project "a" rooted in a temp dir, through the
    /// same entry point `aster run` uses, and return the results by address.
    fn run_targets(
        tmp: &Path,
        targets: Vec<(&str, Target)>,
        requested: &[&str],
    ) -> (Vec<ExecutionResult>, HashMap<String, ExecutionResult>) {
        let mut project = make_project_with_targets(
            "a",
            targets
                .into_iter()
                .map(|(name, target)| (name.to_string(), target))
                .collect(),
        );
        project.root = tmp.to_path_buf();
        let projects = [&project];
        let requested: HashSet<String> = requested.iter().map(|t| format!("//a:{t}")).collect();
        let executor =
            Executor::with_all_options(tmp, crate::cli::output::OutputMode::Json, false, false);
        let results = executor.execute_targets(&requested, &projects, false);
        let by_address = results
            .iter()
            .map(|result| (result.address.clone(), result.clone()))
            .collect();
        (results, by_address)
    }

    /// Shell snippet that succeeds once `file` exists and fails after about
    /// five seconds. Targets use it to wait for another target's marker, so
    /// the test asserts on ordering instead of elapsed time.
    fn wait_for(file: &str) -> String {
        format!(
            "i=0; while [ ! -f {file} ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done; [ -f {file} ]"
        )
    }

    #[test]
    fn dependent_starts_while_unrelated_lower_level_target_still_runs() {
        // A (level 0) can only pass if C (level 1, behind the fast B) has
        // started while A is still running. Under a level barrier C waits
        // for A, A waits for C, and A fails.
        let tmp = tempfile::tempdir().unwrap();
        let (_, by_address) = run_targets(
            tmp.path(),
            vec![
                (
                    "slow",
                    target(&format!("sh -c '{}'", wait_for("c_started")), vec![]),
                ),
                ("fast", target("touch b_done", vec![])),
                ("after_fast", target("touch c_started", vec!["//a:fast"])),
            ],
            &["slow", "after_fast"],
        );

        assert_eq!(by_address.len(), 3);
        for result in by_address.values() {
            assert!(result.success, "{}: {}", result.address, result.output);
        }
    }

    #[test]
    fn failure_blocks_transitive_dependents_but_not_independent_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let (results, by_address) = run_targets(
            tmp.path(),
            vec![
                ("root", target("sh -c 'exit 3'", vec![])),
                ("middle", target("touch middle_ran", vec!["//a:root"])),
                ("leaf", target("touch leaf_ran", vec!["//a:middle"])),
                ("other", target("touch other_ran", vec![])),
            ],
            &["leaf", "other"],
        );

        assert_eq!(results.len(), 4);
        assert!(!by_address["//a:root"].success);
        assert_eq!(by_address["//a:root"].exit_code, Some(3));
        assert!(by_address["//a:other"].success);
        assert!(tmp.path().join("other_ran").exists());
        assert!(by_address["//a:middle"]
            .output
            .contains("Blocked: prerequisite target(s) failed: //a:root"));
        assert!(by_address["//a:leaf"]
            .output
            .contains("Blocked: prerequisite target(s) failed: //a:middle"));
        assert!(!by_address["//a:middle"].success && !by_address["//a:leaf"].success);
        assert!(!tmp.path().join("middle_ran").exists());
        assert!(!tmp.path().join("leaf_ran").exists());
    }

    #[test]
    fn results_are_grouped_by_level_with_blocked_targets_first() {
        let tmp = tempfile::tempdir().unwrap();
        let (results, _) = run_targets(
            tmp.path(),
            vec![
                ("slow", target("sleep 0.3", vec![])),
                ("bad", target("sh -c 'exit 1'", vec![])),
                ("blocked", target("true", vec!["//a:bad"])),
                ("ok", target("true", vec!["//a:slow"])),
            ],
            &["blocked", "ok"],
        );

        let order: Vec<&str> = results.iter().map(|r| r.address.as_str()).collect();
        assert_eq!(order.len(), 4);
        assert!(order[..2].contains(&"//a:slow") && order[..2].contains(&"//a:bad"));
        assert_eq!(order[2], "//a:blocked");
        assert_eq!(order[3], "//a:ok");
    }

    #[test]
    fn dependency_cycle_still_runs_every_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (results, _) = run_targets(
            tmp.path(),
            vec![
                ("x", target("true", vec!["//a:y"])),
                ("y", target("true", vec!["//a:x"])),
            ],
            &["x"],
        );

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.success));
    }

    /// Command that logs `start_<name>`, waits for `until` to exist, then
    /// logs `end_<name>`. One appended line per event keeps the log in the
    /// order the events happened.
    fn critical_section(name: &str, until: Option<&str>) -> String {
        let wait = until
            .map(wait_for)
            .unwrap_or_else(|| "sleep 0.05".to_string());
        format!(
            "sh -c 'echo start_{name} >> events; touch up_{name}; {wait}; echo end_{name} >> events'"
        )
    }

    /// Events that must read `start_x end_x` pairs: no section interleaves.
    fn assert_sections_never_interleave(events: &str, names: &[&str]) {
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), names.len() * 2, "every target ran: {lines:?}");
        for pair in lines.chunks(2) {
            let name = pair[0].strip_prefix("start_").expect("a start comes first");
            assert_eq!(
                pair[1],
                format!("end_{name}"),
                "sections interleaved: {lines:?}"
            );
        }
        for name in names {
            assert!(
                lines.contains(&format!("start_{name}").as_str()),
                "{name} ran"
            );
        }
    }

    /// `first` (level 0) holds its section until `first_waits_for` exists. `second`
    /// depends on `gate`, so it becomes ready while `first` still holds the
    /// section, and `third` follows at level 2.
    fn first_second_third(
        resource: Option<&str>,
        first_waits_for: &str,
    ) -> Vec<(&'static str, Target)> {
        let resources: Vec<&str> = resource.into_iter().collect();
        vec![
            (
                "first",
                target_with_resources(
                    &critical_section("first", Some(first_waits_for)),
                    vec![],
                    resources.clone(),
                ),
            ),
            ("gate", target("touch gate_done", vec![])),
            (
                "second",
                target_with_resources(
                    &critical_section("second", None),
                    vec!["//a:gate"],
                    resources.clone(),
                ),
            ),
            (
                "third",
                target_with_resources(
                    &critical_section("third", None),
                    vec!["//a:second"],
                    resources,
                ),
            ),
        ]
    }

    #[test]
    fn exclusive_resource_sections_never_interleave_across_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let (results, _) = run_targets(
            tmp.path(),
            first_second_third(Some("shared_test_postgres"), "gate_done"),
            &["first", "third"],
        );

        assert!(results.iter().all(|r| r.success), "{results:?}");
        let events = std::fs::read_to_string(tmp.path().join("events")).unwrap();
        assert_sections_never_interleave(&events, &["first", "second", "third"]);
    }

    #[test]
    fn without_the_shared_resource_the_same_sections_do_overlap() {
        // Positive control for the test above: with no resource, `first`
        // stays in its section until `second` has started, so the log
        // interleaves. This is what the resource lock has to prevent.
        let tmp = tempfile::tempdir().unwrap();
        let (results, _) = run_targets(
            tmp.path(),
            first_second_third(None, "up_second"),
            &["first", "third"],
        );

        assert!(results.iter().all(|r| r.success), "{results:?}");
        let events = std::fs::read_to_string(tmp.path().join("events")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        let position = |event: &str| lines.iter().position(|l| *l == event).unwrap();
        assert!(
            position("start_second") < position("end_first"),
            "{lines:?}"
        );
    }

    #[test]
    fn targets_holding_several_resources_do_not_deadlock_or_overlap() {
        // Declared in opposite orders; the executor locks them sorted.
        // Every section is a start/end pair in the log, and the run ends.
        let tmp = tempfile::tempdir().unwrap();
        let section = |name: &str, deps: Vec<&str>, resources: Vec<&str>| {
            target_with_resources(&critical_section(name, None), deps, resources)
        };
        let (results, _) = run_targets(
            tmp.path(),
            vec![
                ("ab", section("ab", vec![], vec!["res_b", "res_a"])),
                ("ba", section("ba", vec![], vec!["res_a", "res_b"])),
                ("only_a", section("only_a", vec![], vec!["res_a"])),
                ("only_b", section("only_b", vec![], vec!["res_b"])),
                (
                    "late",
                    section("late", vec!["//a:ab"], vec!["res_b", "res_a"]),
                ),
            ],
            &["ab", "ba", "only_a", "only_b", "late"],
        );

        assert!(results.iter().all(|r| r.success), "{results:?}");
        let events = std::fs::read_to_string(tmp.path().join("events")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), 10, "every target ran: {lines:?}");
        let span = |name: &str| {
            let at = |event: String| lines.iter().position(|l| *l == event).unwrap();
            (at(format!("start_{name}")), at(format!("end_{name}")))
        };
        // Sections of targets that share a resource must not overlap.
        let holders: [(&str, &[&str]); 5] = [
            ("ab", &["res_a", "res_b"]),
            ("ba", &["res_a", "res_b"]),
            ("only_a", &["res_a"]),
            ("only_b", &["res_b"]),
            ("late", &["res_a", "res_b"]),
        ];
        for (i, (left, left_resources)) in holders.iter().enumerate() {
            for (right, right_resources) in &holders[i + 1..] {
                if left_resources.iter().any(|r| right_resources.contains(r)) {
                    let (ls, le) = span(left);
                    let (rs, re) = span(right);
                    assert!(
                        le < rs || re < ls,
                        "{left} and {right} overlapped: {lines:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn panicking_target_is_reported_blocks_dependents_and_frees_its_resource() {
        let tmp = tempfile::tempdir().unwrap();
        test_hooks::panic_on("//a:boom");
        let (results, by_address) = run_targets(
            tmp.path(),
            vec![
                (
                    "boom",
                    target_with_resources("touch boom_ran", vec![], vec!["res"]),
                ),
                ("after", target("touch after_ran", vec!["//a:boom"])),
                (
                    "sibling",
                    target_with_resources("touch sibling_ran", vec![], vec!["res"]),
                ),
            ],
            &["after", "sibling"],
        );

        assert_eq!(
            results.len(),
            3,
            "the run terminated with every target reported"
        );
        assert!(!by_address["//a:boom"].success);
        assert!(by_address["//a:boom"].output.contains("panicked"));
        assert!(by_address["//a:after"]
            .output
            .contains("Blocked: prerequisite target(s) failed: //a:boom"));
        assert!(
            by_address["//a:sibling"].success,
            "the resource was released"
        );
        assert!(!tmp.path().join("boom_ran").exists());
        assert!(!tmp.path().join("after_ran").exists());
    }

    #[test]
    fn cache_hit_and_skipped_target_each_release_their_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let mut gen_target = target("touch gen_ran", vec![]);
        gen_target.cache = Some(CacheConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let mut project = make_project_with_targets(
            "a",
            HashMap::from([
                ("gen".to_string(), gen_target),
                (
                    "use_gen".to_string(),
                    target("touch use_gen_ran", vec!["//a:gen"]),
                ),
                ("skipped".to_string(), target("touch skipped_ran", vec![])),
                (
                    "use_skipped".to_string(),
                    target("touch use_skipped_ran", vec!["//a:skipped"]),
                ),
            ]),
        );
        project.root = tmp.path().to_path_buf();
        let projects = [&project];
        let executor = Executor::with_output_mode(tmp.path(), crate::cli::output::OutputMode::Json);
        let requested = HashSet::from(["//a:use_gen".to_string()]);

        // Warm the cache, then clear the markers.
        assert!(executor
            .execute_targets(&requested, &projects, false)
            .iter()
            .all(|r| r.success));
        for marker in ["gen_ran", "use_gen_ran"] {
            std::fs::remove_file(tmp.path().join(marker)).unwrap();
        }

        let results = executor.execute_targets(&requested, &projects, false);
        let gen_result = results.iter().find(|r| r.address == "//a:gen").unwrap();
        assert!(gen_result.cached, "second run hits the cache");
        assert!(results.iter().all(|r| r.success));
        assert!(!tmp.path().join("gen_ran").exists());
        assert!(
            tmp.path().join("use_gen_ran").exists(),
            "cache hit released its dependent"
        );

        let overrides = HashMap::from([(
            "//a:skipped".to_string(),
            CommandOverride::Skip("no affected files".to_string()),
        )]);
        let results =
            executor.execute_with_command_overrides("use_skipped", &projects, &overrides, None);
        let skipped = results.iter().find(|r| r.address == "//a:skipped").unwrap();
        assert!(skipped.skipped);
        assert!(!tmp.path().join("skipped_ran").exists());
        assert!(
            tmp.path().join("use_skipped_ran").exists(),
            "skip released its dependent"
        );
    }

    #[test]
    fn different_exclusive_resources_run_at_the_same_time() {
        // Each target waits for the other's marker, so both pass only if
        // they run concurrently.
        let tmp = tempfile::tempdir().unwrap();
        let rendezvous =
            |mine: &str, theirs: &str| format!("sh -c 'touch {mine}; {}'", wait_for(theirs));
        let (_, by_address) = run_targets(
            tmp.path(),
            vec![
                (
                    "left",
                    target_with_resources(
                        &rendezvous("left_up", "right_up"),
                        vec![],
                        vec!["res_a"],
                    ),
                ),
                (
                    "right",
                    target_with_resources(
                        &rendezvous("right_up", "left_up"),
                        vec![],
                        vec!["res_b"],
                    ),
                ),
            ],
            &["left", "right"],
        );

        for result in by_address.values() {
            assert!(result.success, "{}: {}", result.address, result.output);
        }
    }

    #[test]
    fn ready_queue_releases_targets_as_dependencies_complete() {
        let mut targets = HashMap::new();
        targets.insert("a".to_string(), target("true", vec![]));
        targets.insert("b".to_string(), target("true", vec![]));
        targets.insert("c".to_string(), target("true", vec!["//p:b"]));
        targets.insert("d".to_string(), target("true", vec!["//p:a", "//p:c"]));
        let project = make_project_with_targets("p", targets);
        let project_map = HashMap::from([("//p".to_string(), &project)]);
        let set: HashSet<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|t| format!("//p:{t}"))
            .collect();
        let levels = compute_target_levels(&set, &project_map);
        let level_of: HashMap<String, usize> = levels
            .iter()
            .enumerate()
            .flat_map(|(level, addrs)| addrs.iter().map(move |addr| (addr.clone(), level)))
            .collect();

        let mut queue = ReadyQueue::new(&set, &project_map, &level_of);
        assert_eq!(queue.take_ready(), vec!["//p:a", "//p:b"]);
        assert!(queue.take_ready().is_empty());
        queue.complete("//p:b");
        assert_eq!(queue.take_ready(), vec!["//p:c"]);
        queue.complete("//p:a");
        assert!(queue.take_ready().is_empty(), "d still waits for c");
        queue.complete("//p:c");
        assert_eq!(queue.take_ready(), vec!["//p:d"]);
        assert!(queue.take_stuck().is_empty());
    }

    #[test]
    fn test_execution_result_struct() {
        let result = ExecutionResult {
            address: "//test:build".to_string(),
            success: true,
            skipped: false,
            cached: false,
            output: "test output".to_string(),
            log: b"test output".to_vec(),
            duration_ms: 100,
            exit_code: None,
            tests: None,
            input_hash: None,
        };

        assert_eq!(result.address, "//test:build");
        assert!(result.success);
        assert!(!result.skipped);
        assert!(!result.cached);
        assert_eq!(result.output, "test output");
        assert_eq!(result.duration_ms, 100);
    }

    #[test]
    fn captured_execution_closes_stdin_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Executor::new(tmp.path()).null_stdin);
        assert!(
            Executor::with_output_mode(tmp.path(), crate::cli::output::OutputMode::Json).null_stdin
        );
        assert!(
            Executor::with_options(tmp.path(), crate::cli::output::OutputMode::Json, false)
                .null_stdin
        );
        assert!(
            Executor::with_all_options(
                tmp.path(),
                crate::cli::output::OutputMode::Json,
                false,
                false
            )
            .null_stdin
        );
        assert!(Executor::new(tmp.path()).with_null_stdin().null_stdin);
    }

    #[cfg(unix)]
    #[test]
    fn captured_command_stdin_is_not_a_tty() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_command(
            "//a:prompt",
            "sh -c 'if [ -t 0 ]; then echo tty; else echo not-a-tty; fi'",
            &[],
            tmp.path(),
            true,
            DEFAULT_LOG_CAP_BYTES,
        );
        assert!(result.success, "command failed: {}", result.output);
        assert!(
            result.output.contains("not-a-tty"),
            "expected closed stdin, got: {}",
            result.output
        );
    }

    #[cfg(unix)]
    #[test]
    fn override_commands_run_in_order_and_stop_at_the_first_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let invocations: Vec<Invocation> = [
            "sh -c 'echo first'",
            "sh -c 'echo second; exit 3'",
            "sh -c 'echo third'",
        ]
        .into_iter()
        .map(Invocation::plain)
        .collect();
        let (result, raw) = run_invocations(
            "//a:test",
            &invocations,
            tmp.path(),
            true,
            DEFAULT_LOG_CAP_BYTES,
        );
        assert!(!result.success);
        assert_eq!(raw.len(), 2, "the third command never ran");
        assert_eq!(result.exit_code, Some(3));
        assert!(result.output.contains("$ sh -c 'echo first'\nfirst"));
        assert!(result.output.contains("second"));
        assert!(!result.output.contains("third"), "{}", result.output);
    }

    #[cfg(unix)]
    #[test]
    fn invocation_runs_its_rewrite_with_its_env_and_logs_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let invocations = vec![
            Invocation::plain("sh -c 'echo plain'"),
            Invocation {
                shown: "make test".to_string(),
                command: "sh -c 'echo \"report=$ASTER_TEST_REPORT\"'".to_string(),
                env: vec![("ASTER_TEST_REPORT".to_string(), "on".to_string())],
                keep_output: true,
                rewrite: None,
            },
        ];
        let (result, raw) = run_invocations(
            "//a:test",
            &invocations,
            tmp.path(),
            true,
            DEFAULT_LOG_CAP_BYTES,
        );
        assert!(result.success, "{}", result.output);
        assert_eq!(raw, vec![None, Some("report=on\n".to_string())]);
        assert_eq!(result.exit_code, Some(0));
        assert!(
            result.output.contains("$ make test\nreport=on"),
            "{}",
            result.output
        );
    }

    #[cfg(unix)]
    #[test]
    fn go_json_output_is_rendered_for_the_log_and_kept_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let event = r#"{"Action":"output","Package":"p","Output":"ok  \tp\t0.01s\n"}"#;
        let invocation = Invocation {
            shown: "go test ./...".to_string(),
            command: format!("printf '%s\\n' '{event}'"),
            env: Vec::new(),
            keep_output: true,
            rewrite: Some(OutputRewrite::GoTestJson { verbose: false }),
        };
        let (result, raw) = run_invocations(
            "//a:test",
            &[invocation],
            tmp.path(),
            true,
            DEFAULT_LOG_CAP_BYTES,
        );
        assert!(result.success, "{}", result.output);
        assert_eq!(result.output, "ok  \tp\t0.01s\n");
        assert_eq!(raw, vec![Some(format!("{event}\n"))]);
    }

    struct JunitPlugin;

    impl LanguagePlugin for JunitPlugin {
        fn name(&self) -> &str {
            "junit-fixture"
        }
        fn marker_files(&self) -> &[&str] {
            &[]
        }
        fn parse_project(
            &self,
            _root: &Path,
            _config_path: &Path,
        ) -> anyhow::Result<crate::plugins::ProjectMetadata> {
            unreachable!()
        }
        fn parse_dependencies(
            &self,
            _config_path: &Path,
        ) -> anyhow::Result<Vec<crate::plugins::LocalDependency>> {
            unreachable!()
        }
        fn detect_targets(
            &self,
            _ctx: &crate::plugins::TargetContext,
        ) -> anyhow::Result<HashMap<String, Target>> {
            unreachable!()
        }
        /// Plans only commands starting with `report`; they write a JUnit
        /// file into the report directory named by `$REPORT`.
        fn test_report_plan(&self, ctx: &ReportContext) -> Option<ReportPlan> {
            let rest = ctx.command.strip_prefix("report ")?;
            let file = ctx.report_dir.join("junit.xml");
            Some(ReportPlan {
                command: rest.to_string(),
                env: vec![("REPORT".to_string(), file.display().to_string())],
                files: crate::test_results::ReportFiles::Paths(vec![file]),
                format: crate::test_results::ReportFormat::JUnitXml,
                confidence: Confidence::Complete,
                rewrite: None,
                relative_to: None,
                declared: false,
            })
        }
    }

    const WRITE_REPORT: &str = r#"report sh -c 'printf "%s" "<testsuite><testcase classname=\"A\" name=\"ok\"/><testcase classname=\"A\" name=\"bad\"><failure message=\"boom\"/></testcase></testsuite>" > "$REPORT"; exit 1'"#;

    fn capture_and_run(
        tmp: &Path,
        commands: &[String],
    ) -> (ExecutionResult, Vec<Option<(ReportPlan, PathBuf)>>) {
        let captures = plan_captures(&JunitPlugin, None, tmp, tmp, "test", commands);
        let invocations: Vec<Invocation> = commands
            .iter()
            .zip(&captures)
            .map(|(command, capture)| match capture {
                Some((plan, _)) => Invocation::planned(command, plan),
                None => Invocation::plain(command),
            })
            .collect();
        let started = SystemTime::now();
        let (mut result, raw) =
            run_invocations("//a:test", &invocations, tmp, true, DEFAULT_LOG_CAP_BYTES);
        result.tests = read_test_report("//a:test", &captures, &raw, started);
        (result, captures)
    }

    #[cfg(unix)]
    #[test]
    fn captured_report_is_read_and_its_directory_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let (result, captures) = capture_and_run(tmp.path(), &[WRITE_REPORT.to_string()]);
        assert!(!result.success);
        let report = result.tests.expect("a report");
        assert_eq!(report.capture, "junit");
        assert_eq!(report.confidence, Confidence::Complete);
        assert_eq!(report.cases.len(), 2);
        assert_eq!(report.failures(), 1);
        let (_, dir) = captures[0].as_ref().unwrap();
        assert!(!dir.exists(), "report directory is cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn unplanned_or_unrun_commands_make_the_report_partial() {
        let tmp = tempfile::tempdir().unwrap();
        // The first command fails, so the second never runs.
        let (result, _) = capture_and_run(
            tmp.path(),
            &[WRITE_REPORT.to_string(), "report sh -c 'true'".to_string()],
        );
        assert_eq!(result.tests.unwrap().confidence, Confidence::Partial);

        // One command the plugin cannot capture.
        let (result, _) = capture_and_run(
            tmp.path(),
            &[
                "sh -c 'true'".to_string(),
                "report sh -c 'true'".to_string(),
            ],
        );
        let report = result.tests.unwrap();
        assert_eq!(report.confidence, Confidence::Partial);
        assert!(report.cases.is_empty());
    }

    #[test]
    fn a_split_target_reads_a_fixed_declared_report_once() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = ["run a".to_string(), "run b".to_string()];
        let at_path = crate::config::ReportConfig {
            format: crate::config::ReportConfigFormat::Junit,
            files: vec!["target/junit.xml".to_string()],
        };
        // Both commands write the same file: only the last one's is read,
        // and the unread first command makes the results partial.
        let captures = plan_captures(
            &JunitPlugin,
            Some(&at_path),
            tmp.path(),
            tmp.path(),
            "test",
            &commands,
        );
        assert!(captures[0].is_none());
        assert!(captures[1].as_ref().is_some_and(|(plan, _)| plan.declared));
        for (_, dir) in captures.iter().flatten() {
            let _ = std::fs::remove_dir_all(dir);
        }

        // Into the report directory, each command has its own.
        let into_dir = crate::config::ReportConfig {
            files: Vec::new(),
            ..at_path
        };
        let captures = plan_captures(
            &JunitPlugin,
            Some(&into_dir),
            tmp.path(),
            tmp.path(),
            "test",
            &commands,
        );
        let dirs: Vec<&PathBuf> = captures.iter().flatten().map(|(_, dir)| dir).collect();
        assert_eq!(dirs.len(), 2);
        assert_ne!(dirs[0], dirs[1]);
        for (_, dir) in captures.iter().flatten() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn no_plan_means_no_report() {
        let tmp = tempfile::tempdir().unwrap();
        let captures = plan_captures(
            &JunitPlugin,
            None,
            tmp.path(),
            tmp.path(),
            "test",
            &["true".to_string()],
        );
        assert!(captures.iter().all(Option::is_none));
        assert!(read_test_report("//a:test", &captures, &[None], SystemTime::now()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn captured_command_reports_real_exit_code_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_command(
            "//a:test",
            "sh -c 'echo hi; exit 3'",
            &[],
            tmp.path(),
            true,
            DEFAULT_LOG_CAP_BYTES,
        );
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(result.log, b"hi\n");
        assert_eq!(result.output, "hi\n");
    }

    #[cfg(unix)]
    #[test]
    fn captured_log_over_cap_is_truncated_with_marker() {
        let tmp = tempfile::tempdir().unwrap();
        // 20 bytes on stdout, then 20 on stderr. Digits only, so the shell
        // cannot expand them. A cap of 8 keeps whichever head arrives first.
        let result = run_command(
            "//a:test",
            "sh -c 'printf 0123456789abcdefghij; printf 0123456789ABCDEFGHIJ >&2'",
            &[],
            tmp.path(),
            true,
            8,
        );
        assert!(result.success, "{}", result.output);
        assert_eq!(result.exit_code, Some(0));
        let log = String::from_utf8(result.log).unwrap();
        let marker = "\n[aster] log truncated: ";
        let (head, tail) = log.split_once(marker).expect(&log);
        assert_eq!(head.len(), 8, "{log:?}");
        let stdout = b"0123456789abcdefghij";
        let stderr = b"0123456789ABCDEFGHIJ";
        assert!(
            stdout.starts_with(head.as_bytes()) || stderr.starts_with(head.as_bytes()),
            "kept head was not the start of either stream: {log:?}"
        );
        let omitted: u64 = tail
            .strip_suffix(" bytes omitted\n")
            .expect(&log)
            .parse()
            .unwrap();
        assert_eq!(omitted, 32, "{log:?}");
        assert_eq!(result.output, log);
    }

    #[cfg(unix)]
    #[test]
    fn signalled_command_reports_128_plus_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_command(
            "//a:test",
            "sh -c 'kill -TERM $$'",
            &[],
            tmp.path(),
            true,
            DEFAULT_LOG_CAP_BYTES,
        );
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(128 + libc::SIGTERM));
    }

    #[cfg(unix)]
    #[test]
    fn latest_json_records_real_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let mut targets = HashMap::new();
        targets.insert(
            "test".to_string(),
            target("sh -c 'printf 0123456789abcdefghij; exit 3'", vec![]),
        );
        let mut project = make_project_with_targets("a", targets);
        project.root = tmp.path().to_path_buf();
        let requested = HashSet::from(["//a:test".to_string()]);
        let executor = Executor::with_all_options(
            tmp.path(),
            crate::cli::output::OutputMode::Normal,
            false,
            false,
        )
        .with_log_cap(4);
        let projects = [&project];
        let results = executor.execute_targets(&requested, &projects, false);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_code, Some(3));
        assert!(results[0].log.starts_with(b"0123"));
        assert!(results[0].output.contains("log truncated"));

        let stored = LogStore::new(tmp.path()).load_latest().unwrap().unwrap();
        assert_eq!(stored.results.len(), 1);
        assert_eq!(stored.results[0].exit_code, Some(3));
        assert_eq!(stored.results[0].status, "failed");
        assert!(stored.results[0].output.contains("[aster] log truncated"));
    }
}
