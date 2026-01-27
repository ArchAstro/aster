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
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

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
}

impl<'a> Executor<'a> {
    /// Create a new executor with default output mode (Normal)
    pub fn new(workspace_root: &'a Path) -> Self {
        Self {
            workspace_root,
            output_mode: OutputMode::Normal,
            full_logs: false,
            use_cache: true,
        }
    }

    /// Create a new executor with specified output mode
    pub fn with_output_mode(workspace_root: &'a Path, output_mode: OutputMode) -> Self {
        Self {
            workspace_root,
            output_mode,
            full_logs: false,
            use_cache: true,
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
        }
    }

    /// Execute a target on selected projects in dependency order
    ///
    /// Respects target dependencies from Target.depends_on (fully resolved by TargetResolver).
    /// Targets are grouped into DAG levels and each level is executed in parallel.
    /// Output is buffered per-target and printed as a group when complete.
    /// Execution continues on failure, collecting all results.
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
        use std::process::Stdio;

        let project_addr = format!("//{}", project.relative_path.display());
        let target_addr = format!("{project_addr}:{target}");

        // Get the command for this target
        let command = match project.targets.get(target) {
            Some(t) => &t.command,
            None => {
                return Err(format!("No '{target}' target defined for {project_addr}"));
            }
        };

        if self.output_mode == OutputMode::Verbose {
            eprintln!("[aster] Running: {command}");
            eprintln!("[aster] Working directory: {}", project.root.display());
        }

        // Split command by whitespace
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return Err("Empty command".to_string());
        }

        let program = parts[0];
        let args = &parts[1..];

        // Run with inherited stdio for streaming
        let status = Command::new(program)
            .args(args)
            .current_dir(&project.root)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to execute {target_addr}: {e}"))?;

        Ok(status.code().unwrap_or(1))
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

        // Determine if we should show progress output
        // Show in Normal or Verbose mode (ProgressDisplay handles terminal vs CI mode internally)
        let show_progress = matches!(self.output_mode, OutputMode::Normal | OutputMode::Verbose);
        let verbose_progress = self.output_mode == OutputMode::Verbose;
        let show_output = matches!(self.output_mode, OutputMode::Normal | OutputMode::Verbose);

        // Create progress display (verbose mode shows each item with PASS/FAIL/SKIP)
        let mut progress = ProgressDisplay::with_verbose(show_progress, verbose_progress);

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

        // Compute DAG levels based on target dependencies
        let levels = compute_target_levels(&targets_to_run, &project_map);

        // Set total count for progress display
        progress.set_total(targets_to_run.len());

        // Initialize cache infrastructure
        let cache_store = if self.use_cache {
            Some(CacheStore::new(self.workspace_root))
        } else {
            None
        };
        let plugin_registry = PluginRegistry::with_all_plugins();

        // Track computed hashes for hash chaining (shared across levels)
        let computed_hashes: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Capture current environment for cache key computation
        let env_snapshot: HashMap<String, String> = std::env::vars().collect();

        let mut all_results = Vec::new();

        // Execute each level in parallel
        for level in levels {
            let level_results = self.execute_target_level(
                &level,
                &project_map,
                &mut progress,
                show_progress,
                command_overrides,
                cache_store.as_ref(),
                &plugin_registry,
                &computed_hashes,
                &env_snapshot,
            );
            all_results.extend(level_results);
        }

        // Finish the progress display
        progress.finish();

        // Store logs (in all modes except JSON, which is meant for machine processing)
        if self.output_mode != OutputMode::Json {
            self.store_logs(target, &all_results);
        }

        // Print failure details (only in Normal or Verbose mode)
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

        let mut handles = Vec::new();

        for target_addr in target_addrs {
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

            // Check cache if enabled
            if let Some(store) = cache_store {
                if let Some((cache_hit, _current_hash)) = check_cache(
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
                        // Cache miss - store hash for later update after execution
                        // The hash will be stored if execution succeeds
                    }
                }
            }

            // Add spinner for this target
            if show_progress {
                progress.add_running(target_addr);
            }

            let addr = target_addr.clone();
            let project_root = project.root.clone();
            let tx_clone = tx.clone();
            let computed_hashes_clone = Arc::clone(computed_hashes);
            let cache_store_path = cache_store.map(|_| self.workspace_root.to_path_buf());
            let target_name_clone = target_name.clone();
            let command_clone = command.clone();
            let plugin_name = project.plugin_name.clone();
            let env_snapshot_clone = env_snapshot.clone();
            // Get target's depends_on for dependency hash lookup
            let target_deps: Vec<String> = project
                .targets
                .get(&target_name)
                .map(|t| t.depends_on.clone())
                .unwrap_or_default();
            // Get user cache config
            let target_cache_config = project
                .targets
                .get(&target_name)
                .and_then(|t| t.cache.clone());
            // Get invalidates_cache flag for cache invalidation after execution
            let invalidates_cache = project
                .targets
                .get(&target_name)
                .map(|t| t.invalidates_cache)
                .unwrap_or(false);

            let handle = thread::spawn(move || {
                let result = run_command(&addr, &command_clone, &project_root);

                // Update cache if execution succeeded and caching is enabled
                if result.success && cache_store_path.is_some() {
                    if let Some(ref workspace_root) = cache_store_path {
                        // Compute hash for storing
                        let plugin_reg = PluginRegistry::with_all_plugins();
                        if let Some(plugin) = plugin_reg.find_by_name(&plugin_name) {
                            let plugin_inputs = plugin.cache_inputs(&target_name_clone);
                            let hasher = CacheHasher::new(&project_root);

                            // Get dependency hashes from previously computed targets
                            let hashes_guard = computed_hashes_clone.lock().unwrap();
                            let dep_hashes: Vec<&str> = target_deps
                                .iter()
                                .filter_map(|dep| hashes_guard.get(dep).map(|s| s.as_str()))
                                .collect();

                            if let Ok(hash) = hasher.compute_hash(
                                &plugin_inputs,
                                target_cache_config.as_ref(),
                                &command_clone,
                                &dep_hashes,
                                &env_snapshot_clone,
                            ) {
                                drop(hashes_guard);

                                // Store hash
                                {
                                    let mut hashes = computed_hashes_clone.lock().unwrap();
                                    hashes.insert(addr.clone(), hash.clone());
                                }

                                // Update cache store
                                let store = CacheStore::new(&workspace_root);
                                let entry = CacheEntry {
                                    hash,
                                    timestamp: Utc::now().to_rfc3339(),
                                    success: true,
                                };
                                if let Err(e) = store.set(&addr, entry) {
                                    eprintln!(
                                        "[aster] Warning: Failed to update cache for {addr}: {e}"
                                    );
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

/// Parse a target address like "//path/to/project:target" into (project_addr, target_name)
fn parse_target_address(addr: &str) -> Option<(String, String)> {
    let colon_pos = addr.rfind(':')?;
    let project_addr = addr[..colon_pos].to_string();
    let target_name = addr[colon_pos + 1..].to_string();
    Some((project_addr, target_name))
}

/// Recursively collect all target dependencies for a given target
///
/// Follows Target.depends_on which should already be fully resolved by TargetResolver
/// (including cross-project :build dependencies).
fn collect_target_deps(
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
fn compute_target_levels(
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
    // Get plugin for cache inputs
    let plugin = plugin_registry.find_by_name(&project.plugin_name)?;
    let plugin_inputs = plugin.cache_inputs(target_name);

    // If plugin provides no cache inputs, don't cache this target
    if plugin_inputs.source_globs.is_empty()
        && plugin_inputs.config_files.is_empty()
        && plugin_inputs.env_vars.is_empty()
    {
        return None;
    }

    // Get user cache config from project's target configuration
    let target = project.targets.get(target_name)?;
    let user_config: Option<&CacheConfig> = target.cache.as_ref();

    // Get dependency hashes from previously computed targets
    let hashes_guard = computed_hashes.lock().ok()?;
    let dep_hashes: Vec<&str> = target
        .depends_on
        .iter()
        .filter_map(|dep| hashes_guard.get(dep).map(|s| s.as_str()))
        .collect();

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
        if entry.hash == current_hash && entry.success {
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

/// Run a command in a directory and capture output
fn run_command(address: &str, command: &str, working_dir: &Path) -> ExecutionResult {
    let start = Instant::now();

    // Split command by whitespace (simple parsing for v1)
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return ExecutionResult {
            address: address.to_string(),
            success: false,
            skipped: false,
            cached: false,
            output: "Empty command".to_string(),
            duration_ms: 0,
        };
    }

    let program = parts[0];
    let args = &parts[1..];

    let result = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .output();

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
        let projects = vec![project];

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
        let projects = vec![project];

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
        let projects = vec![project_a, project_b];

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
        let projects = vec![project];

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
        let projects = vec![project];

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
