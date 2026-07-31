//! Parallel command execution with grouped output
//!
//! Executes commands in dependency order using DAG levels based on target dependencies:
//! - Collects all target dependencies transitively (e.g., test depends on deps)
//! - Level 0: targets with no dependencies
//! - Level 1: targets depending only on level 0
//! - Level N: targets depending on level 0..N-1
//!
//! Each level executes in parallel, waiting for completion before the next level.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use console::style;

use crate::cache::{CacheEntry, CacheHasher, CacheStore};
use crate::cli::OutputMode;
use crate::config::CacheConfig;
use crate::discovery::DiscoveredProject;
use crate::executor::logs::{LogStore, RunLog, TargetLog};
use crate::graph::ProjectGraph;
use crate::plugins::PluginRegistry;
use crate::ui::ProgressDisplay;

use super::command::parse_command;
use super::shutdown_requested;
#[cfg(any(unix, windows))]
use super::{register_supervised_child, unregister_supervised_child};

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
    /// Combined stdout+stderr output
    pub output: String,
    /// Execution duration in milliseconds
    pub duration_ms: u128,
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
    /// Whether child targets should receive a closed stdin.
    null_stdin: bool,
}

impl<'a> Executor<'a> {
    /// Create a new executor with default output mode (Normal)
    pub fn new(workspace_root: &'a Path) -> Self {
        Self {
            workspace_root,
            output_mode: OutputMode::Normal,
            full_logs: false,
            use_cache: true,
            null_stdin: false,
        }
    }

    /// Create a new executor with specified output mode
    pub fn with_output_mode(workspace_root: &'a Path, output_mode: OutputMode) -> Self {
        Self {
            workspace_root,
            output_mode,
            full_logs: false,
            use_cache: true,
            null_stdin: false,
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
            null_stdin: false,
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
            null_stdin: false,
        }
    }

    /// Prevent targets from reading the caller's terminal.
    ///
    /// This is used for dev-service prerequisites, which execute in a worker
    /// while the dashboard remains the foreground terminal process.
    pub fn with_null_stdin(mut self) -> Self {
        self.null_stdin = true;
        self
    }

    /// Execute a target on selected projects in dependency order
    ///
    /// Respects target dependencies from Target.depends_on (fully resolved by TargetResolver).
    /// Targets are grouped into DAG levels and each level is executed in parallel.
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
        command_overrides: &HashMap<String, String>,
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
        command_overrides: Option<&HashMap<String, String>>,
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

    /// Shared pipeline: compute DAG levels from `targets_to_run`, run each level
    /// in parallel with caching, then finalize progress + logs.
    fn execute_target_set(
        &self,
        targets_to_run: &HashSet<String>,
        project_map: &HashMap<String, &DiscoveredProject>,
        command_overrides: Option<&HashMap<String, String>>,
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

        let levels = compute_target_levels(targets_to_run, project_map);

        progress.set_total(targets_to_run.len());

        let cache_store = if self.use_cache {
            Some(CacheStore::new(self.workspace_root))
        } else {
            None
        };
        let plugin_registry = PluginRegistry::with_all_plugins();

        let computed_hashes: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let env_snapshot: HashMap<String, String> = std::env::vars().collect();

        let mut all_results = Vec::new();
        let mut failed_or_blocked: HashSet<String> = HashSet::new();

        for level in levels {
            if shutdown_requested() {
                break;
            }
            let mut runnable = Vec::new();
            let mut blocked_results = Vec::new();

            for target_addr in level {
                let failed_dependencies = target_dependencies(&target_addr, project_map)
                    .into_iter()
                    .filter(|dependency| failed_or_blocked.contains(dependency))
                    .collect::<Vec<_>>();

                if failed_dependencies.is_empty() {
                    runnable.push(target_addr);
                    continue;
                }

                if show_progress {
                    progress.add_running(&target_addr);
                    progress.mark_complete(&target_addr, false, false, 0);
                }
                failed_or_blocked.insert(target_addr.clone());
                blocked_results.push(ExecutionResult {
                    address: target_addr,
                    success: false,
                    skipped: false,
                    cached: false,
                    output: format!(
                        "Blocked: prerequisite target(s) failed: {}",
                        failed_dependencies.join(", ")
                    ),
                    duration_ms: 0,
                });
            }

            let level_results = self.execute_target_level(
                &runnable,
                project_map,
                &mut progress,
                show_progress,
                command_overrides,
                cache_store.as_ref(),
                &plugin_registry,
                &computed_hashes,
                &env_snapshot,
            );
            failed_or_blocked.extend(
                level_results
                    .iter()
                    .filter(|result| !result.success)
                    .map(|result| result.address.clone()),
            );
            all_results.extend(blocked_results);
            all_results.extend(level_results);
        }

        progress.finish();

        if self.output_mode != OutputMode::Json {
            self.store_logs(target_label_for_logs, &all_results);
        }

        if show_output {
            self.print_failure_details(&all_results, &progress);
        }

        all_results
    }

    /// Execute all targets in a single level in parallel
    #[allow(clippy::too_many_arguments)]
    fn execute_target_level(
        &self,
        target_addrs: &[String],
        project_map: &HashMap<String, &DiscoveredProject>,
        progress: &mut ProgressDisplay,
        show_progress: bool,
        command_overrides: Option<&HashMap<String, String>>,
        cache_store: Option<&CacheStore>,
        plugin_registry: &PluginRegistry,
        computed_hashes: &Arc<Mutex<HashMap<String, String>>>,
        env_snapshot: &HashMap<String, String>,
    ) -> Vec<ExecutionResult> {
        let (tx, rx) = mpsc::channel();

        // Build per-resource mutexes for exclusive access within this level.
        // Targets sharing a resource will serialize; others run in parallel.
        let resource_mutexes: HashMap<String, Arc<Mutex<()>>> = {
            let mut resources = HashSet::new();
            for addr in target_addrs {
                if let Some((pa, tn)) = parse_target_address(addr) {
                    if let Some(p) = project_map.get(&pa) {
                        if let Some(t) = p.targets.get(&tn) {
                            for r in &t.exclusive_resources {
                                resources.insert(r.clone());
                            }
                        }
                    }
                }
            }
            resources
                .into_iter()
                .map(|r| (r, Arc::new(Mutex::new(()))))
                .collect()
        };

        let mut handles = Vec::new();

        for target_addr in target_addrs {
            if shutdown_requested() {
                break;
            }
            // Parse target address: //path/to/project:target_name
            let (project_addr, target_name) = match parse_target_address(target_addr) {
                Some((p, t)) => (p, t),
                None => continue,
            };

            let project = match project_map.get(&project_addr) {
                Some(p) => *p,
                None => continue,
            };

            // Get the command for this target
            // Check for command override first (used with --only-affected-files)
            let command = if let Some(overrides) = command_overrides {
                if let Some(override_cmd) = overrides.get(target_addr) {
                    override_cmd.clone()
                } else {
                    match project.targets.get(&target_name) {
                        Some(t) => t.command.clone(),
                        None => {
                            // No target defined - skip
                            let result = ExecutionResult {
                                address: target_addr.clone(),
                                success: true,
                                skipped: true,
                                cached: false,
                                output: format!("Skipped: no '{target_name}' target defined"),
                                duration_ms: 0,
                            };
                            if show_progress {
                                progress.mark_skipped(target_addr);
                            }
                            let _ = tx.send(result);
                            continue;
                        }
                    }
                }
            } else {
                match project.targets.get(&target_name) {
                    Some(t) => t.command.clone(),
                    None => {
                        // No target defined - skip
                        let result = ExecutionResult {
                            address: target_addr.clone(),
                            success: true, // Not an error, just skipped
                            skipped: true,
                            cached: false,
                            output: format!("Skipped: no '{target_name}' target defined"),
                            duration_ms: 0,
                        };
                        if show_progress {
                            progress.mark_skipped(target_addr);
                        }
                        let _ = tx.send(result);
                        continue;
                    }
                }
            };

            // Check cache if enabled. A miss carries the exact hash that may
            // be stored after successful execution; `None` means this target
            // is not cacheable and must not create a cache entry.
            let mut cache_hash = None;
            if let Some(store) = cache_store {
                if let Some((cache_hit, current_hash)) = check_cache(
                    target_addr,
                    &target_name,
                    &command,
                    project,
                    store,
                    plugin_registry,
                    computed_hashes,
                    env_snapshot,
                ) {
                    if cache_hit {
                        // Cache hit - skip execution
                        let result = ExecutionResult {
                            address: target_addr.clone(),
                            success: true,
                            skipped: false,
                            cached: true,
                            output: String::new(),
                            duration_ms: 0,
                        };
                        if show_progress {
                            progress.mark_cached(target_addr);
                        }
                        let _ = tx.send(result);
                        continue;
                    } else {
                        cache_hash = Some(current_hash);
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
            let tx_clone = tx.clone();
            let computed_hashes_clone = Arc::clone(computed_hashes);
            let cache_store_path = cache_store.map(|_| self.workspace_root.to_path_buf());
            let command_clone = command.clone();
            let target_name_clone = target_name.clone();
            let plugin_name = project.plugin_name.clone();
            let env_snapshot_clone = env_snapshot.clone();
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

            let handle = thread::spawn(move || {
                // Acquire exclusive resource locks (sorted to prevent deadlocks)
                let _resource_guards: Vec<_> = target_resource_locks
                    .iter()
                    .map(|m| m.lock().unwrap())
                    .collect();

                if shutdown_requested() {
                    let result = ExecutionResult {
                        address: addr,
                        success: false,
                        skipped: true,
                        cached: false,
                        output: "Cancelled: shutdown requested".to_string(),
                        duration_ms: 0,
                    };
                    let _ = tx_clone.send(result.clone());
                    return result;
                }
                let mut result = run_command(&addr, &command_clone, &working_dir, null_stdin);
                if shutdown_requested() {
                    result.success = false;
                    result.skipped = true;
                    result.output = if result.output.is_empty() {
                        "Cancelled: shutdown requested".to_string()
                    } else {
                        format!("{}\nCancelled: shutdown requested", result.output)
                    };
                }

                if !result.success {
                    if let Some(workspace_root) = cache_store_path.as_ref() {
                        let store = CacheStore::new(workspace_root);
                        if let Err(e) = store.remove(&addr) {
                            eprintln!(
                                "[aster] Warning: Failed to remove cache after failure for {addr}: {e}"
                            );
                        }
                    }
                } else {
                    // Update cache only when the preflight cache policy
                    // produced a hash for this target.
                    if let (Some(workspace_root), Some(preflight_hash)) =
                        (cache_store_path.as_ref(), cache_hash)
                    {
                        let plugin_registry = PluginRegistry::with_all_plugins();
                        if let Some(plugin) = plugin_registry.find_by_name(&plugin_name) {
                            let plugin_inputs = plugin.cache_inputs(&target_name_clone);
                            let hasher = CacheHasher::new(&project_root);
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
                            );
                            drop(hashes_guard);

                            match postflight_hash {
                                Ok(postflight_hash) if postflight_hash == preflight_hash => {
                                    {
                                        let mut hashes = computed_hashes_clone.lock().unwrap();
                                        hashes.insert(addr.clone(), postflight_hash.clone());
                                    }
                                    let store = CacheStore::new(workspace_root);
                                    let entry = CacheEntry {
                                        hash: postflight_hash,
                                        timestamp: Utc::now().to_rfc3339(),
                                        success: true,
                                    };
                                    if let Err(e) = store.set(&addr, entry) {
                                        eprintln!(
                                            "[aster] Warning: Failed to update cache for {addr}: {e}"
                                        );
                                    }
                                }
                                _ => {
                                    // An unstable or unhashable execution may
                                    // have changed outputs. Do not leave an old
                                    // entry available for a future false hit.
                                    let store = CacheStore::new(workspace_root);
                                    if let Err(e) = store.remove(&addr) {
                                        eprintln!(
                                            "[aster] Warning: Failed to remove stale cache for {addr}: {e}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Invalidate project cache if target has invalidates_cache flag
                if result.success && invalidates_cache {
                    if let Some(ref workspace_root) = cache_store_path {
                        let store = CacheStore::new(workspace_root);
                        // Extract project address from target address (//project:target -> //project)
                        let project_addr =
                            addr.rsplit_once(':').map(|(proj, _)| proj).unwrap_or(&addr);
                        if let Err(e) = store.clear_matching(project_addr) {
                            eprintln!(
                                "[aster] Warning: Failed to invalidate cache for {project_addr}: {e}"
                            );
                        }
                    }
                }

                let _ = tx_clone.send(result.clone());
                result
            });

            handles.push((target_addr.clone(), handle));
        }

        // Drop our sender so rx.iter() completes after all threads finish
        drop(tx);

        // Collect results from channel and update progress
        let mut results = Vec::new();
        for result in rx.iter() {
            if show_progress && !result.cached {
                progress.mark_complete(
                    &result.address,
                    result.success,
                    result.skipped,
                    result.duration_ms,
                );
            }
            results.push(result);
        }

        // Wait for all threads to complete (they should already be done since we drained the channel)
        for (_, handle) in handles {
            let _ = handle.join();
        }

        results
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
                exit_code: if r.skipped {
                    None
                } else {
                    Some(if r.success { 0 } else { 1 })
                },
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

/// Check if a target is cached and return (cache_hit, current_hash)
///
/// Returns Some((true, hash)) if cache hit, Some((false, hash)) if cache miss,
/// or None if caching cannot be performed for this target.
#[allow(clippy::too_many_arguments)]
fn check_cache(
    target_addr: &str,
    target_name: &str,
    command: &str,
    project: &DiscoveredProject,
    cache_store: &CacheStore,
    plugin_registry: &PluginRegistry,
    computed_hashes: &Arc<Mutex<HashMap<String, String>>>,
    env_snapshot: &HashMap<String, String>,
) -> Option<(bool, String)> {
    // Get user cache config from project's target configuration
    let target = project.targets.get(target_name)?;
    let user_config: Option<&CacheConfig> = target.cache.as_ref();
    let explicitly_enabled = user_config.and_then(|config| config.enabled);
    if explicitly_enabled == Some(false)
        || (explicitly_enabled != Some(true) && !is_default_cacheable_target(target_name))
    {
        return None;
    }

    // Get plugin for cache inputs. Targets with no detected inputs are only
    // cacheable by explicit opt-in; their command, dependencies, configured
    // inputs, and environment still form the cache key.
    let plugin = plugin_registry.find_by_name(&project.plugin_name)?;
    let plugin_inputs = plugin.cache_inputs(target_name);
    if plugin_inputs.source_globs.is_empty()
        && plugin_inputs.config_files.is_empty()
        && plugin_inputs.env_vars.is_empty()
        && explicitly_enabled != Some(true)
    {
        return None;
    }

    // Get dependency hashes from previously computed targets
    let hashes_guard = computed_hashes.lock().ok()?;
    let dep_hashes: Vec<&str> = target
        .depends_on
        .iter()
        .map(|dep| hashes_guard.get(dep).map(String::as_str))
        .collect::<Option<_>>()?;

    // Compute current hash
    let hasher = CacheHasher::new(&project.root);
    let current_hash = hasher
        .compute_hash(
            &plugin_inputs,
            user_config,
            command,
            &dep_hashes,
            env_snapshot,
        )
        .ok()?;

    drop(hashes_guard);

    // Check if cached entry matches
    if let Ok(Some(entry)) = cache_store.get(target_addr) {
        let outputs_exist = user_config.is_none_or(|config| {
            config
                .outputs
                .iter()
                .all(|output| project.root.join(output).exists())
        });
        if entry.hash == current_hash && entry.success && outputs_exist {
            // Store computed hash for dependents
            {
                let mut hashes = computed_hashes.lock().ok()?;
                hashes.insert(target_addr.to_string(), current_hash.clone());
            }
            return Some((true, current_hash));
        }
    }

    // Cache miss
    Some((false, current_hash))
}

fn is_default_cacheable_target(target_name: &str) -> bool {
    matches!(
        target_name,
        "deps" | "build" | "test" | "lint" | "format" | "typecheck" | "check"
    )
}

/// Run a command in a directory and capture output
fn run_command(
    address: &str,
    command: &str,
    working_dir: &Path,
    null_stdin: bool,
) -> ExecutionResult {
    let start = Instant::now();

    let parsed = match parse_command(command) {
        Ok(parsed) => parsed,
        Err(error) => {
            return ExecutionResult {
                address: address.to_string(),
                success: false,
                skipped: false,
                cached: false,
                output: error.to_string(),
                duration_ms: 0,
            };
        }
    };

    if parsed.program.is_empty() {
        return ExecutionResult {
            address: address.to_string(),
            success: false,
            skipped: false,
            cached: false,
            output: "Empty command".to_string(),
            duration_ms: 0,
        };
    }

    let mut cmd = Command::new(&parsed.program);
    cmd.args(&parsed.args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if null_stdin {
        cmd.stdin(Stdio::null());
    }

    // Set environment variables
    for (name, value) in parsed.env {
        cmd.env(name, value);
    }

    #[cfg(unix)]
    isolate_streaming_session(&mut cmd);
    #[cfg(windows)]
    prepare_windows_child(&mut cmd);

    #[cfg(any(unix, windows))]
    register_supervised_child();
    let result = cmd
        .spawn()
        .inspect_err(|_| {
            #[cfg(any(unix, windows))]
            unregister_supervised_child();
        })
        .and_then(|child| {
            #[cfg(windows)]
            let mut child = child;
            #[cfg(unix)]
            let monitor = ChildSignalMonitor::new(child.id());
            #[cfg(windows)]
            let monitor = WindowsChildSignalMonitor::new(&mut child)
                .inspect_err(|_| unregister_supervised_child())?;

            let output = child.wait_with_output();

            #[cfg(any(unix, windows))]
            monitor.finish();

            output
        });

    let duration_ms = start.elapsed().as_millis();

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Combine stdout and stderr
            let mut combined = String::new();
            if !stdout.is_empty() {
                combined.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }

            ExecutionResult {
                address: address.to_string(),
                success: output.status.success(),
                skipped: false,
                cached: false,
                output: combined,
                duration_ms,
            }
        }
        Err(e) => ExecutionResult {
            address: address.to_string(),
            success: false,
            skipped: false,
            cached: false,
            output: format!("Failed to execute command: {e}"),
            duration_ms,
        },
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
            relative_path: PathBuf::from(relative_path),
        }
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
        let store = CacheStore::new(tmp.path());
        assert!(store.get("//a:deploy").unwrap().is_none());
        store
            .set(
                "//a:deploy",
                CacheEntry {
                    hash: "stale".to_string(),
                    timestamp: Utc::now().to_rfc3339(),
                    success: true,
                },
            )
            .unwrap();
        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        assert!(store.get("//a:deploy").unwrap().is_none());

        project.targets.get_mut("deploy").unwrap().cache = Some(CacheConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let projects = [&project];
        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        assert!(store.get("//a:deploy").unwrap().is_some());
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
            &CacheStore::new(tmp.path()),
            &PluginRegistry::with_all_plugins(),
            &Arc::new(Mutex::new(HashMap::new())),
            &HashMap::new(),
        );

        assert!(
            result.is_none(),
            "a dependency omitted from the cache key must disable caching"
        );
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
        let store = CacheStore::new(tmp.path());
        store
            .set(
                "//a:build",
                CacheEntry {
                    hash: "old-hash".to_string(),
                    timestamp: Utc::now().to_rfc3339(),
                    success: true,
                },
            )
            .unwrap();

        let results = executor.execute_targets(&requested, &projects, false);
        assert!(results.iter().all(|result| result.success));
        assert!(store.get("//a:build").unwrap().is_none());
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
        let store = CacheStore::new(tmp.path());
        store
            .set(
                "//a:build",
                CacheEntry {
                    hash: "old-hash".to_string(),
                    timestamp: Utc::now().to_rfc3339(),
                    success: true,
                },
            )
            .unwrap();

        let projects = [&project];
        let requested = HashSet::from(["//a:build".to_string()]);
        let results = Executor::with_output_mode(tmp.path(), crate::cli::output::OutputMode::Json)
            .execute_targets(&requested, &projects, false);

        assert!(results.iter().any(|result| !result.success));
        assert!(store.get("//a:build").unwrap().is_none());
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
        }
    }

    #[test]
    fn test_exclusive_resources_serializes_contending_targets() {
        // Two targets sharing "hex_registry" resource should not run concurrently.
        // We verify by having each target sleep briefly, then checking that total
        // duration indicates serialization rather than parallelism.
        let tmp = std::env::temp_dir().join("aster_test_exclusive");
        let _ = std::fs::create_dir_all(&tmp);

        // Use atomic counter to track max concurrent executions
        // The commands will write to files; we check timing via duration
        let mut targets_a = HashMap::new();
        targets_a.insert(
            "deps".to_string(),
            target_with_resources("sleep 0.15", vec![], vec!["shared_cache"]),
        );
        let mut targets_b = HashMap::new();
        targets_b.insert(
            "deps".to_string(),
            target_with_resources("sleep 0.15", vec![], vec!["shared_cache"]),
        );

        let project_a = DiscoveredProject {
            root: tmp.clone(),
            config_path: tmp.join("mix.exs"),
            metadata: ProjectMetadata {
                name: "a".to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: vec![],
            targets: targets_a,
            plugin_name: "elixir".to_string(),
            relative_path: PathBuf::from("a"),
        };
        let project_b = DiscoveredProject {
            root: tmp.clone(),
            config_path: tmp.join("mix.exs"),
            metadata: ProjectMetadata {
                name: "b".to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: vec![],
            targets: targets_b,
            plugin_name: "elixir".to_string(),
            relative_path: PathBuf::from("b"),
        };

        let projects = [project_a, project_b];
        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let executor = Executor::new(&tmp);
        let target_addrs = vec!["//a:deps".to_string(), "//b:deps".to_string()];
        let mut progress = crate::ui::ProgressDisplay::new(false);
        let computed_hashes = Arc::new(Mutex::new(HashMap::new()));
        let env_snapshot = HashMap::new();
        let plugin_registry = crate::plugins::PluginRegistry::with_all_plugins();

        let start = Instant::now();
        let results = executor.execute_target_level(
            &target_addrs,
            &project_map,
            &mut progress,
            false,
            None,
            None,
            &plugin_registry,
            &computed_hashes,
            &env_snapshot,
        );
        let total_ms = start.elapsed().as_millis();

        // Both targets should succeed
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.success, "Target {} should succeed", r.address);
        }

        // If serialized, total time >= 300ms (2 x 150ms).
        // If parallel, total time would be ~150ms.
        assert!(
            total_ms >= 280,
            "Contending targets should serialize (took {total_ms}ms, expected >= 280ms)"
        );
    }

    #[test]
    fn test_exclusive_resources_non_contending_run_parallel() {
        // Two targets with DIFFERENT resources should run in parallel
        let tmp = std::env::temp_dir().join("aster_test_exclusive_parallel");
        let _ = std::fs::create_dir_all(&tmp);

        let mut targets_a = HashMap::new();
        targets_a.insert(
            "deps".to_string(),
            target_with_resources("sleep 0.5", vec![], vec!["resource_a"]),
        );
        let mut targets_b = HashMap::new();
        targets_b.insert(
            "deps".to_string(),
            target_with_resources("sleep 0.5", vec![], vec!["resource_b"]),
        );

        let project_a = DiscoveredProject {
            root: tmp.clone(),
            config_path: tmp.join("mix.exs"),
            metadata: ProjectMetadata {
                name: "a".to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: vec![],
            targets: targets_a,
            plugin_name: "elixir".to_string(),
            relative_path: PathBuf::from("a"),
        };
        let project_b = DiscoveredProject {
            root: tmp.clone(),
            config_path: tmp.join("mix.exs"),
            metadata: ProjectMetadata {
                name: "b".to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: vec![],
            targets: targets_b,
            plugin_name: "elixir".to_string(),
            relative_path: PathBuf::from("b"),
        };

        let projects = [project_a, project_b];
        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), p))
            .collect();

        let executor = Executor::new(&tmp);
        let target_addrs = vec!["//a:deps".to_string(), "//b:deps".to_string()];
        let mut progress = crate::ui::ProgressDisplay::new(false);
        let computed_hashes = Arc::new(Mutex::new(HashMap::new()));
        let env_snapshot = HashMap::new();
        let plugin_registry = crate::plugins::PluginRegistry::with_all_plugins();

        let start = Instant::now();
        let results = executor.execute_target_level(
            &target_addrs,
            &project_map,
            &mut progress,
            false,
            None,
            None,
            &plugin_registry,
            &computed_hashes,
            &env_snapshot,
        );
        let total_ms = start.elapsed().as_millis();

        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.success, "Target {} should succeed", r.address);
        }

        // Non-contending targets should run in parallel (~500ms, not ~1000ms).
        // Leave enough headroom for process startup on loaded hosted runners.
        assert!(
            total_ms < 900,
            "Non-contending targets should run in parallel (took {total_ms}ms, expected < 900ms)"
        );
    }

    #[test]
    fn test_execution_result_struct() {
        let result = ExecutionResult {
            address: "//test:build".to_string(),
            success: true,
            skipped: false,
            cached: false,
            output: "test output".to_string(),
            duration_ms: 100,
        };

        assert_eq!(result.address, "//test:build");
        assert!(result.success);
        assert!(!result.skipped);
        assert!(!result.cached);
        assert_eq!(result.output, "test output");
        assert_eq!(result.duration_ms, 100);
    }
}
