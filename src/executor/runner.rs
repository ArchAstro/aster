//! Parallel command execution with grouped output
//!
//! Executes commands in dependency order using DAG levels:
//! - Level 0: projects with no dependencies
//! - Level 1: projects depending only on level 0
//! - Level N: projects depending on level 0..N-1
//!
//! Each level executes in parallel, waiting for completion before the next level.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;

/// Result of executing a target command on a project
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Project address (//path/to/project)
    pub address: String,
    /// Whether the command succeeded (exit code 0)
    pub success: bool,
    /// Combined stdout+stderr output
    pub output: String,
    /// Execution duration in milliseconds
    pub duration_ms: u128,
}

/// Executor for running target commands on projects
pub struct Executor<'a> {
    /// Workspace root directory (reserved for future use)
    #[allow(dead_code)]
    workspace_root: &'a Path,
}

impl<'a> Executor<'a> {
    /// Create a new executor
    pub fn new(workspace_root: &'a Path) -> Self {
        Self { workspace_root }
    }

    /// Execute a target on selected projects in dependency order
    ///
    /// Projects are grouped into DAG levels and each level is executed in parallel.
    /// Output is buffered per-project and printed as a group when complete.
    /// Execution continues on failure, collecting all results.
    pub fn execute(
        &self,
        target: &str,
        projects: &[&DiscoveredProject],
        _graph: &ProjectGraph,
    ) -> Vec<ExecutionResult> {
        if projects.is_empty() {
            return Vec::new();
        }

        // Build address -> project map
        let project_map: HashMap<String, &DiscoveredProject> = projects
            .iter()
            .map(|p| (format!("//{}", p.relative_path.display()), *p))
            .collect();

        // Compute DAG levels
        let levels = compute_levels(projects, _graph);

        let mut all_results = Vec::new();

        // Execute each level in parallel
        for level in levels {
            let level_results = self.execute_level(target, &level, &project_map);
            all_results.extend(level_results);
        }

        all_results
    }

    /// Execute all projects in a single level in parallel
    fn execute_level(
        &self,
        target: &str,
        addresses: &[String],
        project_map: &HashMap<String, &DiscoveredProject>,
    ) -> Vec<ExecutionResult> {
        let (tx, rx) = mpsc::channel();

        let mut handles = Vec::new();

        for address in addresses {
            let project = match project_map.get(address) {
                Some(p) => *p,
                None => continue,
            };

            // Get the command for this target
            let command = match project.targets.get(target) {
                Some(cmd) => cmd.clone(),
                None => {
                    // No target defined - print message and skip
                    let result = ExecutionResult {
                        address: address.clone(),
                        success: true, // Not an error, just skipped
                        output: format!("Skipped: no '{}' target defined", target),
                        duration_ms: 0,
                    };
                    println!("\n--- {} ---", address);
                    println!("{}", result.output);
                    let _ = tx.send(result);
                    continue;
                }
            };

            let addr = address.clone();
            let project_root = project.root.clone();
            let tx = tx.clone();

            let handle = thread::spawn(move || {
                let result = run_command(&addr, &command, &project_root);

                // Print grouped output
                println!("\n--- {} ---", result.address);
                if !result.output.is_empty() {
                    println!("{}", result.output);
                }
                println!(
                    "[{}] {} ({}ms)",
                    if result.success { "OK" } else { "FAIL" },
                    result.address,
                    result.duration_ms
                );

                let _ = tx.send(result);
            });

            handles.push(handle);
        }

        // Drop our sender so rx.iter() completes after all threads finish
        drop(tx);

        // Wait for all threads
        for handle in handles {
            let _ = handle.join();
        }

        // Collect results
        rx.iter().collect()
    }
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
                output: combined,
                duration_ms,
            }
        }
        Err(e) => ExecutionResult {
            address: address.to_string(),
            success: false,
            output: format!("Failed to execute command: {}", e),
            duration_ms,
        },
    }
}

/// Compute DAG levels for parallel execution
///
/// Level 0 = projects with no dependencies (in selection)
/// Level N = projects whose dependencies are all in levels 0..N-1
fn compute_levels(projects: &[&DiscoveredProject], graph: &ProjectGraph) -> Vec<Vec<String>> {
    // Build set of addresses in selection
    let selected: HashSet<String> = projects
        .iter()
        .map(|p| format!("//{}", p.relative_path.display()))
        .collect();

    // Track which level each project is assigned to
    let mut level_of: HashMap<String, usize> = HashMap::new();

    // Iterate until all projects are assigned
    let mut remaining: HashSet<String> = selected.clone();
    let mut current_level = 0;

    while !remaining.is_empty() {
        let mut this_level = Vec::new();

        for addr in &remaining {
            // Get dependencies that are in our selection
            let deps_in_selection: Vec<String> = graph
                .dependencies(addr)
                .iter()
                .map(|n| n.address.clone())
                .filter(|d| selected.contains(d))
                .collect();

            // If all dependencies are assigned to previous levels, this project goes in current level
            let all_deps_assigned = deps_in_selection
                .iter()
                .all(|d| level_of.get(d).map(|l| *l < current_level).unwrap_or(false));

            // Or if no dependencies in selection
            let no_deps = deps_in_selection.is_empty();

            if all_deps_assigned || no_deps {
                this_level.push(addr.clone());
            }
        }

        // If we made no progress, there's a cycle (shouldn't happen if graph is checked)
        if this_level.is_empty() && !remaining.is_empty() {
            // Fallback: just add remaining projects
            this_level = remaining.iter().cloned().collect();
        }

        // Assign levels and remove from remaining
        for addr in &this_level {
            level_of.insert(addr.clone(), current_level);
            remaining.remove(addr);
        }

        current_level += 1;
        if !this_level.is_empty() {
            // Levels will be collected
        }
    }

    // Build level vectors
    let max_level = level_of.values().max().copied().unwrap_or(0);
    let mut levels: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];

    for (addr, level) in level_of {
        levels[level].push(addr);
    }

    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::plugins::{LocalDependency, ProjectMetadata};
    use std::path::PathBuf;

    fn make_project(
        name: &str,
        relative_path: &str,
        deps: Vec<(&str, &str)>,
        targets: Vec<(&str, &str)>,
    ) -> DiscoveredProject {
        DiscoveredProject {
            root: PathBuf::from(format!("/workspace/{}", relative_path)),
            config_path: PathBuf::from(format!("/workspace/{}/package.json", relative_path)),
            metadata: ProjectMetadata {
                name: name.to_string(),
                version: Some("1.0.0".to_string()),
            },
            dependencies: deps
                .into_iter()
                .map(|(name, path)| LocalDependency {
                    name: name.to_string(),
                    path: PathBuf::from(path),
                })
                .collect(),
            targets: targets
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            plugin_name: "nodejs".to_string(),
            relative_path: PathBuf::from(relative_path),
        }
    }

    #[test]
    fn test_compute_levels_single() {
        let projects = vec![make_project("a", "a", vec![], vec![("test", "echo test")])];
        let refs: Vec<&DiscoveredProject> = projects.iter().collect();
        let graph = build_graph(&projects).unwrap();

        let levels = compute_levels(&refs, &graph);

        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0], vec!["//a".to_string()]);
    }

    #[test]
    fn test_compute_levels_linear_chain() {
        // C depends on B, B depends on A
        let projects = vec![
            make_project("a", "a", vec![], vec![("test", "echo a")]),
            make_project("b", "b", vec![("a", "//a")], vec![("test", "echo b")]),
            make_project("c", "c", vec![("b", "//b")], vec![("test", "echo c")]),
        ];
        let refs: Vec<&DiscoveredProject> = projects.iter().collect();
        let graph = build_graph(&projects).unwrap();

        let levels = compute_levels(&refs, &graph);

        assert_eq!(levels.len(), 3);
        assert!(levels[0].contains(&"//a".to_string()));
        assert!(levels[1].contains(&"//b".to_string()));
        assert!(levels[2].contains(&"//c".to_string()));
    }

    #[test]
    fn test_compute_levels_parallel() {
        // B and C both depend on A (B and C can run in parallel after A)
        let projects = vec![
            make_project("a", "a", vec![], vec![("test", "echo a")]),
            make_project("b", "b", vec![("a", "//a")], vec![("test", "echo b")]),
            make_project("c", "c", vec![("a", "//a")], vec![("test", "echo c")]),
        ];
        let refs: Vec<&DiscoveredProject> = projects.iter().collect();
        let graph = build_graph(&projects).unwrap();

        let levels = compute_levels(&refs, &graph);

        assert_eq!(levels.len(), 2);
        assert!(levels[0].contains(&"//a".to_string()));
        assert!(levels[1].contains(&"//b".to_string()));
        assert!(levels[1].contains(&"//c".to_string()));
    }

    #[test]
    fn test_compute_levels_diamond() {
        // D depends on B and C, both B and C depend on A
        let projects = vec![
            make_project("a", "a", vec![], vec![("test", "echo a")]),
            make_project("b", "b", vec![("a", "//a")], vec![("test", "echo b")]),
            make_project("c", "c", vec![("a", "//a")], vec![("test", "echo c")]),
            make_project(
                "d",
                "d",
                vec![("b", "//b"), ("c", "//c")],
                vec![("test", "echo d")],
            ),
        ];
        let refs: Vec<&DiscoveredProject> = projects.iter().collect();
        let graph = build_graph(&projects).unwrap();

        let levels = compute_levels(&refs, &graph);

        assert_eq!(levels.len(), 3);
        assert!(levels[0].contains(&"//a".to_string()));
        assert!(levels[1].contains(&"//b".to_string()));
        assert!(levels[1].contains(&"//c".to_string()));
        assert!(levels[2].contains(&"//d".to_string()));
    }

    #[test]
    fn test_compute_levels_subset() {
        // Full graph: C -> B -> A
        // But we only select B and A
        let projects = vec![
            make_project("a", "a", vec![], vec![("test", "echo a")]),
            make_project("b", "b", vec![("a", "//a")], vec![("test", "echo b")]),
            make_project("c", "c", vec![("b", "//b")], vec![("test", "echo c")]),
        ];
        let graph = build_graph(&projects).unwrap();

        // Only select A and B
        let subset: Vec<&DiscoveredProject> = projects.iter().take(2).collect();
        let levels = compute_levels(&subset, &graph);

        assert_eq!(levels.len(), 2);
        assert!(levels[0].contains(&"//a".to_string()));
        assert!(levels[1].contains(&"//b".to_string()));
    }

    #[test]
    fn test_execution_result_struct() {
        let result = ExecutionResult {
            address: "//test".to_string(),
            success: true,
            output: "test output".to_string(),
            duration_ms: 100,
        };

        assert_eq!(result.address, "//test");
        assert!(result.success);
        assert_eq!(result.output, "test output");
        assert_eq!(result.duration_ms, 100);
    }
}
