# Phase 3: CLI & Git - Research

**Researched:** 2026-01-22
**Domain:** CLI command execution, git integration, parallel process management, graph path-finding
**Confidence:** HIGH

## Summary

Phase 3 implements the user-facing CLI commands that leverage the graph infrastructure built in Phases 1-2. The core challenges are: (1) implementing git-aware affected detection using the `git2` crate, (2) parallel command execution with grouped output buffering, (3) the "why" command using petgraph's path-finding algorithms, and (4) extending clap to handle targets as dynamic subcommands.

The CONTEXT.md decisions lock in several key behaviors: parallel execution by default with continue-all-on-failure semantics, grouped output buffering per project, and following Nx conventions for flag naming and affected command semantics. Research confirms these are standard patterns in the monorepo tool space.

**Primary recommendation:** Use `git2` for affected detection (diff_tree_to_tree for committed changes, statuses() for uncommitted), `std::process::Command` with thread-per-process for parallel execution with output buffering, and petgraph's `astar` or simple BFS for the "why" command's path finding.

## Standard Stack

The established libraries/tools for this domain:

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| git2 | 0.19.x | Git operations (diff, status, refs) | Official Rust bindings for libgit2, used by cargo |
| clap | 4.5.x | CLI with external subcommand support | Already in Cargo.toml, supports dynamic subcommands |
| petgraph | 0.8.x | Graph path-finding (astar, has_path_connecting) | Already in Cargo.toml, has all needed algorithms |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| std::process | - | Command execution | Running target commands (npm test, mix test, etc.) |
| std::thread | - | Parallel execution | Spawning concurrent command processes |
| std::sync::mpsc | - | Output collection | Gathering buffered output from parallel commands |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| git2 | gitoxide | gitoxide is newer/purer-Rust but less battle-tested, git2 has libgit2's 10+ years of edge cases |
| std::thread | tokio | Async adds complexity; command execution is I/O-bound but simple spawn/wait is sufficient |
| std::thread | rayon | rayon is for data parallelism; command execution needs individual process control |

**Installation:**
```toml
# Add to existing Cargo.toml
[dependencies]
git2 = "0.19"
# clap, petgraph already present
```

## Architecture Patterns

### Recommended Project Structure
```
src/
├── cli/
│   ├── mod.rs           # CLI module (existing)
│   ├── commands.rs      # Command definitions (extend)
│   └── run.rs           # NEW: Target execution logic
├── git/
│   ├── mod.rs           # NEW: Git module
│   ├── affected.rs      # NEW: Affected detection logic
│   └── file_owner.rs    # NEW: Map files to projects
├── graph/
│   ├── mod.rs           # (existing)
│   ├── builder.rs       # (existing)
│   ├── cycles.rs        # (existing)
│   └── path.rs          # NEW: Path finding for "why"
└── executor/
    ├── mod.rs           # NEW: Command execution
    ├── parallel.rs      # NEW: Parallel runner with output buffering
    └── output.rs        # NEW: Output collection and formatting
```

### Pattern 1: Target as External Subcommand

**What:** Allow arbitrary targets (test, build, lint, custom) as subcommands without hardcoding each one.

**When to use:** For the `aster <target> [projects...]` command pattern.

**Example:**
```rust
// Source: clap external subcommand pattern
use clap::{Parser, Subcommand, CommandFactory, FromArgMatches};

#[derive(Parser)]
#[command(name = "aster")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all discovered projects
    List,

    /// Show dependency graph
    Graph {
        project: Option<String>,
    },

    /// Show dependency path between two projects
    Why {
        /// Source project (//path/to/project)
        from: String,
        /// Target project (//path/to/project)
        to: String,
    },

    /// Run on projects affected by git changes
    Affected {
        /// Target to run (test, build, lint, etc.)
        target: String,

        /// Base ref for comparison (default: main)
        #[arg(long, default_value = "main")]
        base: String,

        /// Head ref for comparison (default: HEAD + uncommitted)
        #[arg(long)]
        head: Option<String>,
    },

    /// Initialize aster workspace
    Init,

    /// Run a target on projects (catch-all for targets)
    #[command(external_subcommand)]
    Run(Vec<String>),
}
```

### Pattern 2: Git Affected Detection

**What:** Use git2 to find changed files between refs and map them to affected projects.

**When to use:** For `aster affected <target>` command.

**Example:**
```rust
// Source: git2 docs + Nx affected semantics
use git2::{Repository, Diff, DiffOptions, Status, StatusOptions};
use std::path::{Path, PathBuf};
use std::collections::HashSet;

pub struct AffectedDetector<'a> {
    repo: &'a Repository,
    workspace_root: &'a Path,
}

impl<'a> AffectedDetector<'a> {
    /// Get files changed between base..head refs
    pub fn changed_files_between_refs(
        &self,
        base: &str,
        head: &str,
    ) -> anyhow::Result<HashSet<PathBuf>> {
        let base_obj = self.repo.revparse_single(base)?;
        let head_obj = self.repo.revparse_single(head)?;

        let base_tree = base_obj.peel_to_tree()?;
        let head_tree = head_obj.peel_to_tree()?;

        let diff = self.repo.diff_tree_to_tree(
            Some(&base_tree),
            Some(&head_tree),
            None,
        )?;

        let mut changed = HashSet::new();
        for delta in diff.deltas() {
            if let Some(path) = delta.new_file().path() {
                changed.insert(path.to_path_buf());
            }
            if let Some(path) = delta.old_file().path() {
                changed.insert(path.to_path_buf());
            }
        }

        Ok(changed)
    }

    /// Get uncommitted changes (staged + unstaged)
    pub fn uncommitted_changes(&self) -> anyhow::Result<HashSet<PathBuf>> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true);

        let statuses = self.repo.statuses(Some(&mut opts))?;
        let mut changed = HashSet::new();

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                changed.insert(PathBuf::from(path));
            }
        }

        Ok(changed)
    }

    /// Combine ref diff with uncommitted (Nx default behavior)
    pub fn all_affected_files(
        &self,
        base: &str,
        head: Option<&str>,
    ) -> anyhow::Result<HashSet<PathBuf>> {
        let mut changed = if let Some(h) = head {
            // Explicit head: only compare refs, no uncommitted
            self.changed_files_between_refs(base, h)?
        } else {
            // No head: compare base..HEAD plus uncommitted
            let mut files = self.changed_files_between_refs(base, "HEAD")?;
            files.extend(self.uncommitted_changes()?);
            files
        };

        Ok(changed)
    }
}
```

### Pattern 3: File-to-Project Mapping

**What:** Determine which project owns a changed file by checking if the file path starts with a project's path.

**When to use:** After getting changed files, map them to affected projects.

**Example:**
```rust
// Source: Nx affected semantics
use std::path::Path;
use crate::graph::ProjectNode;

/// Map changed files to their owning projects
pub fn files_to_projects<'a>(
    changed_files: &HashSet<PathBuf>,
    projects: &'a [ProjectNode],
) -> HashSet<&'a str> {
    let mut affected_addresses = HashSet::new();

    for file in changed_files {
        // Find the project that owns this file
        // A project owns a file if the file is under the project's directory
        for project in projects {
            // Extract path from address (//services/api -> services/api)
            let project_path = project.address.strip_prefix("//").unwrap_or(&project.address);

            if file.starts_with(project_path) {
                affected_addresses.insert(project.address.as_str());
                break; // File belongs to first matching project (most specific)
            }
        }
    }

    affected_addresses
}

/// Get affected projects including dependents (transitive)
pub fn affected_with_dependents<'a>(
    directly_affected: HashSet<&'a str>,
    graph: &'a ProjectGraph,
) -> HashSet<&'a str> {
    use petgraph::Direction;

    let mut all_affected = directly_affected.clone();
    let mut to_process: Vec<_> = directly_affected.into_iter().collect();

    // BFS to find all dependents (projects that depend on affected projects)
    while let Some(addr) = to_process.pop() {
        if let Some(&idx) = graph.index_by_address.get(addr) {
            // Get dependents (nodes with edges pointing TO this node)
            for dependent_idx in graph.graph.neighbors_directed(idx, Direction::Incoming) {
                let dependent = &graph.graph[dependent_idx];
                if all_affected.insert(&dependent.address) {
                    to_process.push(&dependent.address);
                }
            }
        }
    }

    all_affected
}
```

### Pattern 4: Parallel Command Execution with Grouped Output

**What:** Run commands in parallel while buffering output per-project, displaying complete output blocks.

**When to use:** For all target execution.

**Example:**
```rust
// Source: CONTEXT.md decisions + std::process patterns
use std::process::{Command, Stdio, Output};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::collections::HashMap;

pub struct ExecutionResult {
    pub address: String,
    pub success: bool,
    pub output: String,
    pub duration_ms: u128,
}

pub struct ParallelExecutor {
    max_parallel: usize,
}

impl ParallelExecutor {
    /// Execute commands in parallel, respecting dependency order
    ///
    /// Strategy:
    /// 1. Group projects by "level" in the DAG (projects with no deps = level 0)
    /// 2. Execute each level in parallel
    /// 3. Only proceed to next level when current level completes
    pub fn execute_in_order(
        &self,
        projects: Vec<ProjectNode>,
        target: &str,
        target_resolver: &TargetResolver,
        graph: &ProjectGraph,
    ) -> Vec<ExecutionResult> {
        let levels = self.compute_execution_levels(&projects, graph);
        let mut all_results = Vec::new();

        for level in levels {
            let level_results = self.execute_level_parallel(&level, target, target_resolver);
            all_results.extend(level_results);

            // Check for failures but continue (continue-all-on-failure)
            // Just collect failures, report at end
        }

        all_results
    }

    fn execute_level_parallel(
        &self,
        projects: &[&ProjectNode],
        target: &str,
        resolver: &TargetResolver,
    ) -> Vec<ExecutionResult> {
        let (tx, rx): (Sender<ExecutionResult>, Receiver<ExecutionResult>) = channel();

        let mut handles = Vec::new();

        for project in projects.iter().take(self.max_parallel) {
            let tx = tx.clone();
            let address = project.address.clone();
            let root = project.root.clone();
            let cmd = resolver.resolve(&project.plugin_name, target, &project.targets);

            let handle = thread::spawn(move || {
                let start = std::time::Instant::now();

                let result = if let Some(command_str) = cmd {
                    execute_command(&command_str, &root)
                } else {
                    // No command configured for this target
                    ExecutionResult {
                        address: address.clone(),
                        success: true, // Skip is not a failure
                        output: format!("Skipped: no '{}' target configured", target),
                        duration_ms: 0,
                    }
                };

                let _ = tx.send(ExecutionResult {
                    address,
                    duration_ms: start.elapsed().as_millis(),
                    ..result
                });
            });

            handles.push(handle);
        }

        drop(tx); // Close sender so receiver knows when done

        // Collect results
        let mut results = Vec::new();
        for result in rx {
            // Print grouped output as each completes
            print_grouped_output(&result);
            results.push(result);
        }

        // Wait for all threads
        for handle in handles {
            let _ = handle.join();
        }

        results
    }
}

fn execute_command(command_str: &str, working_dir: &Path) -> ExecutionResult {
    // Parse command (simple split, could use shell_words for quoted args)
    let parts: Vec<&str> = command_str.split_whitespace().collect();
    let (program, args) = parts.split_first().unwrap_or((&"", &[]));

    let output = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(out) => ExecutionResult {
            address: String::new(), // Filled in by caller
            success: out.status.success(),
            output: format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            duration_ms: 0,
        },
        Err(e) => ExecutionResult {
            address: String::new(),
            success: false,
            output: format!("Failed to execute: {}", e),
            duration_ms: 0,
        },
    }
}
```

### Pattern 5: "Why" Command Path Finding

**What:** Find and display the dependency path between two projects.

**When to use:** For `aster why //a //b` command.

**Example:**
```rust
// Source: petgraph astar docs
use petgraph::algo::astar;

/// Find the dependency path from source to target
pub fn find_dependency_path(
    graph: &ProjectGraph,
    from_address: &str,
    to_address: &str,
) -> Option<Vec<String>> {
    let from_idx = graph.index_by_address.get(from_address)?;
    let to_idx = graph.index_by_address.get(to_address)?;

    // Use astar with uniform edge cost (we just want any path)
    let result = astar(
        &graph.graph,
        *from_idx,
        |node| node == *to_idx,    // is_goal
        |_edge| 1,                  // edge_cost (uniform)
        |_node| 0,                  // heuristic (none needed)
    );

    result.map(|(_cost, path)| {
        path.into_iter()
            .map(|idx| graph.graph[idx].address.clone())
            .collect()
    })
}

/// Format path for display
pub fn format_dependency_path(path: &[String]) -> String {
    if path.is_empty() {
        return "No path found".to_string();
    }

    path.iter()
        .enumerate()
        .map(|(i, addr)| {
            if i == 0 {
                addr.clone()
            } else {
                format!("  -> {}", addr)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
```

### Anti-Patterns to Avoid

- **Using shell for command execution:** Don't spawn `sh -c "command"`. Use `Command::new()` with explicit program and args. Shell invocation adds overhead and security concerns.

- **Polling for process completion:** Don't use `try_wait()` in a loop. Use `wait_with_output()` or spawn threads that call `wait()`.

- **Mixing stdout/stderr ordering:** When buffering output, capture both stdout and stderr together or you'll get interleaved output in unpredictable order.

- **Ignoring process cleanup:** Always wait on spawned processes. Orphaned processes become zombies and can exhaust system resources.

- **Using git CLI instead of git2:** Shelling out to `git` is slower and harder to parse. git2 provides structured data.

## Don't Hand-Roll

Problems that look simple but have existing solutions:

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Git diff parsing | Shell out to `git diff` | `git2::Repository::diff_tree_to_tree` | Structured data, no parsing needed |
| Ref resolution | `git rev-parse` output parsing | `git2::Repository::revparse_single` | Handles all ref formats (branch, tag, SHA, HEAD~1) |
| Path finding in graph | Custom BFS implementation | `petgraph::algo::astar` | Returns path reconstruction, handles edge cases |
| Command argument parsing | String splitting | `shell_words` crate (if needed) | Handles quotes, escapes correctly |
| Process output capture | Manual stdout/stderr handling | `Command::output()` or `wait_with_output()` | Handles buffering, avoids deadlocks |

**Key insight:** git2's API mirrors git's semantics exactly - `revparse_single` accepts the same rev specs as `git rev-parse`, making it easy to map Nx-style `--base=main --head=HEAD` to git2 calls.

## Common Pitfalls

### Pitfall 1: Output Deadlock with Large Command Output

**What goes wrong:** Command hangs when output exceeds pipe buffer size (~64KB).

**Why it happens:** If you pipe stdout/stderr but don't read them while the process runs, the buffer fills and the process blocks.

**How to avoid:** Use `wait_with_output()` which handles this, or spawn reader threads for stdout/stderr before calling `wait()`.

**Warning signs:** Commands hang on projects with verbose output.

### Pitfall 2: Affected Detection Includes Too Many Projects

**What goes wrong:** Changing one file marks the entire repo as affected.

**Why it happens:** File ownership check uses `starts_with` and the root project (workspace root) matches all files.

**How to avoid:** Sort projects by path length (longest first) and match files to the most specific project. Or exclude the workspace root from project list.

**Warning signs:** `affected` always runs all projects.

### Pitfall 3: Git Ref Resolution Fails

**What goes wrong:** `revparse_single("main")` fails with "reference not found".

**Why it happens:** Repository might use `master` instead of `main`, or the ref might be remote-only (`origin/main`).

**How to avoid:** Try multiple fallbacks: `main` -> `master` -> `origin/main` -> `origin/master`. Or let user specify explicit ref.

**Warning signs:** `affected` command fails on repositories with non-standard branch names.

### Pitfall 4: Dependency Order Violated in Parallel Execution

**What goes wrong:** A project runs before its dependencies finish building.

**Why it happens:** Simple parallel execution doesn't respect the DAG ordering.

**How to avoid:** Compute "levels" in the DAG where each level only contains projects whose dependencies are in earlier levels. Execute levels sequentially, projects within a level in parallel.

**Warning signs:** Build failures that work when run serially.

### Pitfall 5: Windows Path Handling in Git

**What goes wrong:** Affected detection doesn't find projects on Windows.

**Why it happens:** Git uses forward slashes internally, Windows paths use backslashes.

**How to avoid:** Normalize all paths to forward slashes when comparing, or use `Path` APIs that handle this.

**Warning signs:** Works on macOS/Linux, fails on Windows.

### Pitfall 6: Uncommitted Changes Not Detected

**What goes wrong:** User makes changes but `affected` doesn't find them.

**Why it happens:** Only comparing committed refs, not checking `statuses()`.

**How to avoid:** When `--head` is not specified, include both `diff_tree_to_tree(base, HEAD)` AND `statuses()` for uncommitted changes. This matches Nx's default behavior.

**Warning signs:** Users must commit before running `affected`.

## Code Examples

Verified patterns from official sources:

### Git Repository Opening
```rust
// Source: git2 docs
use git2::Repository;

pub fn open_repo(workspace_root: &Path) -> anyhow::Result<Repository> {
    Repository::discover(workspace_root)
        .with_context(|| format!("No git repository found at {}", workspace_root.display()))
}
```

### CLI Argument Parsing for Run Command
```rust
// Source: clap external_subcommand pattern
impl Commands {
    /// Parse run command arguments: target [projects...] [flags]
    pub fn parse_run_args(args: Vec<String>) -> RunArgs {
        let mut projects = Vec::new();
        let mut no_deps = false;
        let mut dependents = false;
        let mut all = false;

        // First arg is target
        let target = args.first().cloned().unwrap_or_default();

        for arg in args.iter().skip(1) {
            match arg.as_str() {
                "--no-deps" => no_deps = true,
                "--dependents" => dependents = true,
                "--all" => all = true,
                s if s.starts_with("//") => projects.push(s.to_string()),
                _ => {} // Unknown flag, ignore or error
            }
        }

        RunArgs {
            target,
            projects,
            no_deps,
            dependents,
            all,
        }
    }
}

pub struct RunArgs {
    pub target: String,
    pub projects: Vec<String>,
    pub no_deps: bool,
    pub dependents: bool,
    pub all: bool,
}
```

### Project Selection Logic
```rust
// Source: Nx run-many semantics
pub fn select_projects<'a>(
    args: &RunArgs,
    graph: &'a ProjectGraph,
    cwd_project: Option<&str>,
) -> Vec<&'a ProjectNode> {
    // --all: run on all projects
    if args.all {
        return graph.projects().collect();
    }

    // Explicit projects specified
    if !args.projects.is_empty() {
        return args.projects.iter()
            .filter_map(|addr| graph.get(addr))
            .collect();
    }

    // No projects specified: use cwd project if in a project directory
    if let Some(addr) = cwd_project {
        if let Some(project) = graph.get(addr) {
            return vec![project];
        }
    }

    // Fallback: error or run on all?
    Vec::new()
}

/// Expand selection based on flags
pub fn expand_selection<'a>(
    mut selected: Vec<&'a ProjectNode>,
    args: &RunArgs,
    graph: &'a ProjectGraph,
) -> Vec<&'a ProjectNode> {
    use petgraph::Direction;

    let mut result = HashSet::new();

    for project in &selected {
        result.insert(&project.address);

        // --dependents: also include projects that depend on selected
        if args.dependents {
            if let Some(&idx) = graph.index_by_address.get(&project.address) {
                for dep_idx in graph.graph.neighbors_directed(idx, Direction::Incoming) {
                    result.insert(&graph.graph[dep_idx].address);
                }
            }
        }

        // Default behavior: include dependencies (unless --no-deps)
        if !args.no_deps {
            if let Some(&idx) = graph.index_by_address.get(&project.address) {
                for dep_idx in graph.graph.neighbors_directed(idx, Direction::Outgoing) {
                    result.insert(&graph.graph[dep_idx].address);
                }
            }
        }
    }

    // Convert back to ProjectNode references and sort by topo order
    graph.topological_order()
        .into_iter()
        .filter(|p| result.contains(&p.address.as_str()))
        .collect()
}
```

### aster init Implementation
```rust
// Source: CONTEXT.md decisions
use std::fs;
use std::path::Path;

pub fn init_workspace(workspace_root: &Path) -> anyhow::Result<()> {
    let aster_toml = workspace_root.join("aster.toml");

    if aster_toml.exists() {
        anyhow::bail!("aster.toml already exists at {}", aster_toml.display());
    }

    // Create minimal aster.toml
    let content = r#"# Aster workspace configuration
# This file marks the workspace root for aster.
# Project-specific configuration goes in aster.toml files in each project directory.

# Uncomment to set workspace-wide defaults:
# [defaults]
# default_target = "build"
"#;

    fs::write(&aster_toml, content)
        .with_context(|| format!("Failed to write {}", aster_toml.display()))?;

    println!("Created {}", aster_toml.display());

    // Scan and report what would be discovered
    // (Using existing discovery infrastructure)
    println!("\nScanning for projects...");

    Ok(())
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Shell out to git CLI | git2/gitoxide crates | Always best practice | Structured data, faster |
| Sequential execution | Parallel with DAG levels | Standard since Nx/Turborepo | 2-10x faster builds |
| Interleaved output | Grouped/buffered output | Nx "static" output style | Readable CI logs |
| Manual ref parsing | git2 revparse_single | git2 has always supported this | Handles all ref formats |

**Deprecated/outdated:**
- `--all` flag in Nx: Deprecated in favor of `nx run-many`. We can keep it for convenience.
- Sequential-only execution: All modern tools default to parallel.

## Open Questions

Things that couldn't be fully resolved:

1. **Default parallelism level**
   - What we know: Nx defaults to 3 parallel processes
   - What's unclear: Is 3 optimal or should we use num_cpus?
   - Recommendation: Default to 3 like Nx, allow `--parallel=N` flag in future (Phase 4)

2. **Shell interpretation for commands**
   - What we know: Target commands like "npm test" need to run in shell on some systems
   - What's unclear: Should we always use shell or direct execution?
   - Recommendation: Start with direct execution (split on whitespace). Add shell fallback if users report issues with complex commands.

3. **Exact Nx affected semantics for edge cases**
   - What we know: Nx uses --base=main --head=HEAD as defaults
   - What's unclear: How does Nx handle merge commits, rebases?
   - Recommendation: Implement basic two-ref comparison first. Real-world testing will reveal edge cases.

4. **Graph output format for `aster graph`**
   - What we know: Current implementation shows simple tree
   - What's unclear: Should we add DOT format for graphviz?
   - Recommendation: Keep simple tree for now. DOT format can be added when users request it.

## Sources

### Primary (HIGH confidence)
- [git2 Repository docs](https://docs.rs/git2/latest/git2/struct.Repository.html) - diff_tree_to_tree, statuses, revparse_single
- [git2 Diff docs](https://docs.rs/git2/latest/git2/struct.Diff.html) - deltas(), foreach()
- [petgraph astar](https://docs.rs/petgraph/latest/petgraph/algo/astar/fn.astar.html) - Path reconstruction
- [petgraph Graph neighbors_directed](https://docs.rs/petgraph/latest/petgraph/graph/struct.Graph.html) - Incoming/Outgoing directions
- [std::process::Child](https://doc.rust-lang.org/std/process/struct.Child.html) - wait_with_output, process management
- [std::process::Command](https://doc.rust-lang.org/std/process/struct.Command.html) - spawn, output
- [clap Command](https://docs.rs/clap/latest/clap/struct.Command.html) - external_subcommands
- [Nx affected docs](https://nx.dev/nx-api/nx/documents/affected) - --base, --head flags
- [Nx run-many docs](https://nx.dev/nx-api/nx/documents/run-many) - --projects, --all, --parallel

### Secondary (MEDIUM confidence)
- [Nx GitHub Issue #18053](https://github.com/nrwl/nx/issues/18053) - Community request for --no-deps flag
- [rust-parallel crate](https://github.com/aaronriekenberg/rust-parallel) - Tokio-based parallel execution patterns
- [shared_child crate](https://docs.rs/shared_child) - Concurrent process handling

### Tertiary (LOW confidence)
- Nx "why" command: Could not find official docs; implementation based on general graph path-finding

## Metadata

**Confidence breakdown:**
- Git integration: HIGH - git2 is battle-tested, API well-documented
- Command execution: HIGH - std::process is stable, patterns well-understood
- CLI design: HIGH - clap external subcommand pattern documented
- Affected detection: HIGH - Nx semantics documented, git2 supports all needed operations
- "Why" command: MEDIUM - petgraph astar is documented but exact UX is our design choice
- Parallel execution levels: MEDIUM - Pattern is standard but exact implementation is custom

**Research date:** 2026-01-22
**Valid until:** 60 days (git2, petgraph, clap are mature and stable)
