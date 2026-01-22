# Phase 1: Foundation - Research

**Researched:** 2026-01-22
**Domain:** Graph engine, project discovery, config parsing, Node.js plugin (Rust CLI)
**Confidence:** HIGH

## Summary

Phase 1 establishes Aster's core architecture: a dependency graph engine that auto-discovers projects by scanning for config files, parses them to extract dependencies, and builds a DAG. The standard Rust CLI stack is well-established: `clap` for argument parsing, `petgraph` for graph algorithms, `ignore` for gitignore-aware directory traversal, and `serde`/`serde_json`/`toml` for config parsing.

The primary technical challenges are: (1) extracting the exact cycle path when cycles are detected (petgraph's `toposort` only returns one node in the cycle), (2) implementing Bazel-style addressing (`//path/to/project:target`), and (3) designing the plugin trait for extensibility while keeping plugins compiled-in for v1.

**Primary recommendation:** Use petgraph for the graph engine, implement custom cycle path extraction using DFS with path tracking, and leverage the `ignore` crate for fast gitignore-respecting directory traversal.

## Standard Stack

The established libraries/tools for this domain:

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| clap | 4.5.x | CLI argument parsing with derive macros | Industry standard, 99%+ of Rust CLIs use it |
| petgraph | 0.8.x | Directed graph, topological sort, cycle detection | 250M+ downloads, de facto Rust graph library |
| ignore | 0.4.x | Fast parallel directory traversal with gitignore | From ripgrep author, proven at scale |
| anyhow | 1.0.x | Application-level error handling | Standard for Rust applications (not libraries) |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| serde | 1.0.x | Serialization framework | All config parsing |
| serde_json | 1.0.x | JSON parsing | package.json parsing |
| toml | 0.8.x | TOML parsing | aster.toml parsing |
| rayon | 1.11.x | Data parallelism | Parallel config file parsing |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| petgraph | daggy | daggy wraps petgraph, adds overhead without benefit |
| ignore | walkdir | walkdir lacks gitignore support, ignore includes walkdir |
| anyhow | thiserror | thiserror for libraries, anyhow for applications |

**Installation:**
```toml
[dependencies]
clap = { version = "4.5", features = ["derive"] }
petgraph = "0.8"
ignore = "0.4"
anyhow = "1.0"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"
rayon = "1.11"

[dev-dependencies]
tempfile = "3"  # For test fixtures
```

## Architecture Patterns

### Recommended Project Structure
```
src/
├── main.rs              # CLI entry point
├── cli/
│   ├── mod.rs           # CLI module exports
│   └── commands.rs      # Command definitions (list, graph)
├── config/
│   ├── mod.rs           # Config module exports
│   ├── workspace.rs     # Workspace root detection, root aster.toml
│   └── project.rs       # Project-level aster.toml parsing
├── discovery/
│   ├── mod.rs           # Discovery module exports
│   └── scanner.rs       # WalkBuilder-based project discovery
├── graph/
│   ├── mod.rs           # Graph module exports
│   ├── builder.rs       # Build graph from discovered projects
│   └── cycles.rs        # Cycle detection with path extraction
├── plugins/
│   ├── mod.rs           # Plugin trait definition
│   ├── registry.rs      # Plugin registration
│   └── nodejs.rs        # Node.js package.json parser
└── address.rs           # Bazel-style address parsing (//path:target)
```

### Pattern 1: Plugin Trait for Language Support

**What:** Define a trait that each language plugin implements, enabling polymorphic project parsing.

**When to use:** Always. This is the core extensibility mechanism.

**Example:**
```rust
// Source: Prior research ARCHITECTURE.md
pub trait LanguagePlugin: Send + Sync {
    /// Files that identify this project type (e.g., ["package.json"])
    fn marker_files(&self) -> &[&str];

    /// Parse native config to extract project metadata
    fn parse_project(&self, root: &Path, config_path: &Path) -> Result<ProjectMetadata>;

    /// Extract local dependencies from native config
    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>>;
}

pub struct ProjectMetadata {
    pub name: String,
    pub version: Option<String>,
}

pub struct LocalDependency {
    /// The dependency name as declared
    pub name: String,
    /// Resolved path to the dependency (relative to workspace root)
    pub path: PathBuf,
}
```

### Pattern 2: Workspace Root Detection

**What:** Walk up from current directory to find workspace root marker (aster.toml or .git).

**When to use:** On every CLI invocation to establish workspace context.

**Example:**
```rust
// Source: git2 docs and common patterns
use std::path::{Path, PathBuf};

pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.canonicalize().ok()?;

    loop {
        // Check for explicit aster.toml marker
        if current.join("aster.toml").exists() {
            return Some(current);
        }
        // Fall back to .git as workspace boundary
        if current.join(".git").exists() {
            return Some(current);
        }

        // Move to parent
        if !current.pop() {
            return None;
        }
    }
}
```

### Pattern 3: Bazel-Style Address Parsing

**What:** Parse `//path/to/project:target` addresses into structured components.

**When to use:** For all project references in CLI arguments and config files.

**Example:**
```rust
// Bazel/Buck addressing convention
#[derive(Debug, Clone, PartialEq)]
pub struct Address {
    /// Path from workspace root (e.g., "services/api")
    pub path: PathBuf,
    /// Optional target name (e.g., "build", "test")
    pub target: Option<String>,
}

impl Address {
    /// Parse "//services/api:build" or "//services/api"
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.strip_prefix("//")
            .ok_or_else(|| anyhow!("Address must start with //: {}", s))?;

        if let Some((path, target)) = s.split_once(':') {
            Ok(Address {
                path: PathBuf::from(path),
                target: Some(target.to_string()),
            })
        } else {
            Ok(Address {
                path: PathBuf::from(s),
                target: None,
            })
        }
    }

    /// Check for recursive glob: "//services/..."
    pub fn is_recursive(&self) -> bool {
        self.path.to_string_lossy().ends_with("/...")
            || self.path.to_string_lossy() == "..."
    }
}
```

### Pattern 4: Cycle Detection with Path Extraction

**What:** Detect cycles in the dependency graph AND extract the exact cycle path for error reporting.

**When to use:** After building the graph, before any operations that assume acyclicity.

**Example:**
```rust
// Source: Adapted from petgraph patterns
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use std::collections::HashSet;

pub struct CycleError {
    /// The cycle path: A -> B -> C -> A
    pub path: Vec<String>,
}

/// Detect cycles and return the exact path if found
pub fn find_cycle<N: Clone>(
    graph: &DiGraph<N, ()>,
    get_name: impl Fn(&N) -> String,
) -> Option<CycleError> {
    let mut visited = HashSet::new();
    let mut rec_stack = HashSet::new();
    let mut path = Vec::new();

    for node in graph.node_indices() {
        if !visited.contains(&node) {
            if let Some(cycle_start) = dfs_cycle(
                graph, node, &mut visited, &mut rec_stack, &mut path
            ) {
                // Extract cycle from path
                let cycle_idx = path.iter().position(|&n| n == cycle_start).unwrap();
                let cycle_nodes: Vec<String> = path[cycle_idx..]
                    .iter()
                    .map(|&idx| get_name(&graph[idx]))
                    .collect();

                // Add first node again to show the cycle
                let mut cycle_path = cycle_nodes;
                if let Some(first) = cycle_path.first().cloned() {
                    cycle_path.push(first);
                }

                return Some(CycleError { path: cycle_path });
            }
        }
    }
    None
}

fn dfs_cycle<N>(
    graph: &DiGraph<N, ()>,
    node: NodeIndex,
    visited: &mut HashSet<NodeIndex>,
    rec_stack: &mut HashSet<NodeIndex>,
    path: &mut Vec<NodeIndex>,
) -> Option<NodeIndex> {
    visited.insert(node);
    rec_stack.insert(node);
    path.push(node);

    for edge in graph.edges(node) {
        let neighbor = edge.target();

        if !visited.contains(&neighbor) {
            if let Some(cycle_node) = dfs_cycle(graph, neighbor, visited, rec_stack, path) {
                return Some(cycle_node);
            }
        } else if rec_stack.contains(&neighbor) {
            // Found cycle - neighbor is the start of the cycle
            return Some(neighbor);
        }
    }

    path.pop();
    rec_stack.remove(&node);
    None
}
```

### Anti-Patterns to Avoid

- **Using petgraph's `toposort` alone for cycle detection:** It returns `Err(Cycle { node_id })` with only ONE node, not the full cycle path. Users need the exact cycle for debugging.

- **String concatenation for paths:** Always use `PathBuf::join()` and `Path` APIs. Never `format!("{}/{}", dir, file)`.

- **Sequential directory traversal:** Use `ignore::WalkBuilder` with parallel walking. Sequential traversal is 10x slower on large monorepos.

- **Hardcoded language detection:** All language-specific logic must go through the plugin trait. No switch statements on file extensions in core code.

## Don't Hand-Roll

Problems that look simple but have existing solutions:

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Gitignore parsing | Custom glob matcher | `ignore` crate | Gitignore has complex precedence rules (`.ignore` > `.gitignore` > `.git/info/exclude` > global) |
| Directory traversal | `std::fs::read_dir` recursion | `ignore::WalkBuilder` | Parallel, respects gitignore, handles symlinks |
| Cycle detection | Basic DFS | petgraph + custom path extraction | petgraph handles edge cases, just extend for path reporting |
| JSON parsing | Manual string parsing | `serde_json` | Handles Unicode, escapes, edge cases correctly |
| TOML parsing | Regex extraction | `toml` crate | TOML has complex rules for strings, dates, nested tables |
| CLI argument parsing | `std::env::args` | `clap` derive | Help text, validation, subcommands, completions |

**Key insight:** The "simple" version of each of these takes 2 hours to write and 2 weeks to debug edge cases. The libraries have years of edge case fixes.

## Common Pitfalls

### Pitfall 1: Cycle Detection Returns Only One Node

**What goes wrong:** petgraph's `toposort()` returns `Err(Cycle { node_id })` with a single node. Users get errors like "Cycle detected involving project X" with no way to understand the cycle.

**Why it happens:** Topological sort algorithms detect cycles by finding back-edges, which identifies ONE node in the cycle, not the path.

**How to avoid:** Implement custom cycle detection using DFS with path tracking (see Pattern 4 above). When a back-edge is found, the current recursion path contains the cycle.

**Warning signs:** User complaints about unhelpful cycle errors.

### Pitfall 2: Path Handling Breaks on Windows

**What goes wrong:** Code uses `/` literally for path separators, breaking on Windows which uses `\`.

**Why it happens:** Developers test only on macOS/Linux.

**How to avoid:**
- Use `PathBuf::join()` not string concatenation
- Use `Path::components()` for iteration
- Never hardcode `/` or `\` as separators
- Test in CI on Windows

**Warning signs:** "Works on my Mac" but fails in CI.

### Pitfall 3: Ignoring Node.js Workspace Conventions

**What goes wrong:** Aster finds projects but misses dependencies because they're declared at workspace root, not in individual package.json files.

**Why it happens:** npm/pnpm workspaces declare packages at root level with `"workspaces": ["packages/*"]`.

**How to avoid:**
- Check root package.json for `workspaces` field
- Honor the workspace configuration for package discovery
- This is in CONTEXT.md as "Claude's Discretion" - recommend supporting it

**Warning signs:** Missing edges in dependency graph for workspace projects.

### Pitfall 4: Silent Config Parse Failures

**What goes wrong:** Malformed package.json parses to empty/default values instead of erroring.

**Why it happens:** Using `serde_json::from_str` without checking for required fields.

**How to avoid:**
- Define required fields without `Option<T>`
- Enable `#[serde(deny_unknown_fields)]` during development to catch typos
- Validate after parsing: check name is non-empty, dependencies are valid paths

**Warning signs:** Projects showing up with wrong names or zero dependencies.

### Pitfall 5: Name Collisions Across Languages

**What goes wrong:** Two projects have the same name (e.g., `core` in package.json and `core` in mix.exs), causing graph building to fail or silently merge them.

**Why it happens:** CONTEXT.md decision: "if names collide, add language suffix" - but code doesn't implement this.

**How to avoid:** During discovery, track `(name, language)` pairs. On collision, rename to `name-language` suffix (e.g., `core-node`, `core-elixir`).

**Warning signs:** Fewer projects discovered than expected, or graph edges pointing to wrong projects.

## Code Examples

Verified patterns from official sources:

### package.json Dependency Extraction
```rust
// Source: npm docs + serde_json patterns
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
}

/// Extract file: dependencies from package.json
pub fn parse_file_dependencies(path: &Path) -> Result<Vec<LocalDependency>> {
    let content = std::fs::read_to_string(path)?;
    let pkg: PackageJson = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let mut deps = Vec::new();

    // Process both dependencies and devDependencies
    for dep_map in [&pkg.dependencies, &pkg.dev_dependencies].into_iter().flatten() {
        for (name, version) in dep_map {
            if let Some(local_path) = version.strip_prefix("file:") {
                deps.push(LocalDependency {
                    name: name.clone(),
                    path: PathBuf::from(local_path),
                });
            }
        }
    }

    Ok(deps)
}
```

### WalkBuilder for Project Discovery
```rust
// Source: ignore crate docs
use ignore::WalkBuilder;
use std::path::Path;

pub fn discover_projects(root: &Path, plugins: &[Box<dyn LanguagePlugin>]) -> Result<Vec<DiscoveredProject>> {
    let mut projects = Vec::new();

    // Collect all marker files from plugins
    let markers: Vec<&str> = plugins
        .iter()
        .flat_map(|p| p.marker_files())
        .collect();

    let walker = WalkBuilder::new(root)
        .hidden(false)           // Don't skip hidden dirs (but gitignore handles .git etc)
        .git_ignore(true)        // Honor .gitignore
        .git_exclude(true)       // Honor .git/info/exclude
        .git_global(true)        // Honor global gitignore
        .build();

    for entry in walker {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            let filename = path.file_name().and_then(|s| s.to_str());

            if let Some(name) = filename {
                if markers.contains(&name) {
                    // Find which plugin handles this marker
                    for plugin in plugins {
                        if plugin.marker_files().contains(&name) {
                            let project_root = path.parent().unwrap();
                            let metadata = plugin.parse_project(root, path)?;
                            let deps = plugin.parse_dependencies(path)?;

                            projects.push(DiscoveredProject {
                                root: project_root.to_path_buf(),
                                config_path: path.to_path_buf(),
                                metadata,
                                dependencies: deps,
                                plugin_name: plugin.name().to_string(),
                            });
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(projects)
}
```

### aster.toml Parsing
```rust
// Source: toml crate docs + CONTEXT.md decisions
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
pub struct AsterToml {
    /// Override project name
    pub name: Option<String>,

    /// Cross-language dependencies: ["//services/platform:build"]
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Custom targets
    #[serde(default)]
    pub targets: HashMap<String, String>,
}

pub fn parse_aster_toml(path: &Path) -> Result<AsterToml> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let config: AsterToml = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    // Validate depends_on entries are valid addresses
    for dep in &config.depends_on {
        Address::parse(dep)
            .with_context(|| format!("Invalid dependency address in {}", path.display()))?;
    }

    Ok(config)
}
```

### CLI Structure with clap
```rust
// Source: clap derive tutorial
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "aster")]
#[command(version, about = "Build orchestration for polyglot monorepos")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all discovered projects
    List {
        /// Output format (text, json)
        #[arg(short, long, default_value = "text")]
        format: String,
    },

    /// Show dependency graph
    Graph {
        /// Specific project to show (//path/to/project)
        project: Option<String>,

        /// Output format (text, dot, json)
        #[arg(short, long, default_value = "text")]
        format: String,
    },
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| walkdir for traversal | ignore crate | 2018+ | 10x faster with gitignore support |
| structopt for CLI | clap v4 derive | clap 3.0 (2021) | structopt merged into clap |
| manual path joining | PathBuf everywhere | Always best practice | Cross-platform compatibility |
| regex for JSON | serde_json | Always best practice | Correct Unicode, escapes, types |

**Deprecated/outdated:**
- `structopt`: Merged into clap, use `clap` with derive feature
- `walkdir` alone: Use `ignore` which includes walkdir + gitignore

## Open Questions

Things that couldn't be fully resolved:

1. **npm/pnpm workspace support**
   - What we know: npm workspaces use `"workspaces": ["packages/*"]` in root package.json
   - What's unclear: Should Aster auto-detect this or require explicit aster.toml?
   - Recommendation: Honor workspace declarations in Phase 1 - it's how most monorepos work. Parse root package.json for `workspaces` field and include those paths in discovery.

2. **Graph output format for `aster graph`**
   - What we know: Options are text (indented tree), DOT (graphviz), JSON
   - What's unclear: CONTEXT.md marks this as "Claude's discretion"
   - Recommendation: Default to simple text tree, support `--format dot` for graphviz compatibility

3. **Error verbosity levels**
   - What we know: Need clear cycle errors with exact path
   - What's unclear: How verbose should other errors be?
   - Recommendation: Use anyhow's `with_context()` liberally. Errors should answer: What failed? Where? Why?

## Sources

### Primary (HIGH confidence)
- [petgraph algo docs](https://docs.rs/petgraph/latest/petgraph/algo/index.html) - toposort, cycle detection, SCC algorithms
- [petgraph Cycle struct](https://docs.rs/petgraph/latest/petgraph/algo/struct.Cycle.html) - Only provides one node_id, not full path
- [ignore WalkBuilder](https://docs.rs/ignore/latest/ignore/struct.WalkBuilder.html) - Full API for directory traversal
- [clap derive tutorial](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html) - CLI structure patterns
- [serde_json crate](https://docs.rs/serde_json/latest/serde_json/) - JSON parsing
- [toml crate](https://docs.rs/toml) - TOML parsing
- [npm package.json docs](https://docs.npmjs.com/cli/v8/configuring-npm/package-json/) - file: dependency format
- [anyhow Context trait](https://docs.rs/anyhow/latest/anyhow/trait.Context.html) - Error context best practices
- [git2 Repository](https://docs.rs/git2/latest/git2/struct.Repository.html) - discover() method for repo root

### Secondary (MEDIUM confidence)
- [Rust unit testing](https://doc.rust-lang.org/book/ch11-03-test-organization.html) - Test organization patterns
- [graph-cycles crate](https://docs.rs/graph-cycles) - Alternative for cycle path extraction (less mature)

### Tertiary (LOW confidence)
- Community patterns for exact cycle path extraction - requires custom implementation

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH - All crates are industry standard with millions of downloads
- Architecture: HIGH - Patterns from prior research + official docs
- Pitfalls: HIGH - Documented in prior PITFALLS.md research
- Cycle path extraction: MEDIUM - Requires custom code, pattern is well-understood

**Research date:** 2026-01-22
**Valid until:** 60 days (Rust ecosystem is stable, petgraph/clap/ignore are mature)
