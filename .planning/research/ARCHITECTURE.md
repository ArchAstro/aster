# Architecture Research: Build Orchestration CLI

**Project:** Aster
**Researched:** 2026-01-22
**Confidence:** HIGH (verified against Turborepo, Nx, moon, Buck2 architectures)

## Component Overview

```
                          +------------------+
                          |   CLI Entry      |
                          |   (args, config) |
                          +--------+---------+
                                   |
                                   v
                          +------------------+
                          |  Config Loader   |
                          | (workspace.toml) |
                          +--------+---------+
                                   |
                                   v
+------------------+      +------------------+      +------------------+
|  Language Plugin |<---->|   Discovery      |      |   Git Analysis   |
|  Registry        |      |   Engine         |      |   (affected)     |
+------------------+      +--------+---------+      +--------+---------+
        ^                          |                         |
        |                          v                         |
        |                 +------------------+               |
        +---------------->|  Project Graph   |<--------------+
                          |  (DAG Builder)   |
                          +--------+---------+
                                   |
                                   v
                          +------------------+
                          |  Task Graph      |
                          |  (execution DAG) |
                          +--------+---------+
                                   |
                                   v
                          +------------------+
                          |  Task Scheduler  |
                          |  (parallel exec) |
                          +--------+---------+
                                   |
                                   v
                          +------------------+
                          |  Task Runner     |
                          |  (process mgmt)  |
                          +--------+---------+
                                   |
                                   v
                          +------------------+
                          |  Output Handler  |
                          |  (TUI / JSON)    |
                          +------------------+
```

## Components

### 1. CLI Entry (`cli`)

- **Responsibility**: Parse command-line arguments, validate inputs, route to appropriate handlers
- **Inputs**: Raw CLI arguments, environment variables
- **Outputs**: Parsed command structure with validated options
- **Dependencies**: None (entry point)

**Key patterns from Turborepo:**
- Use strongly-typed argument structs (e.g., `RunArgs`, `ListArgs`)
- Minimal parsing first (verbosity, color), then full parsing
- Separate concerns: argument parsing vs command execution
- Return `Result<i32, Error>` where i32 is exit code

**Recommended crate:** `clap` with derive macros

```rust
// Example structure
pub enum Command {
    Run(RunArgs),
    List(ListArgs),
    Graph(GraphArgs),
}

pub struct RunArgs {
    pub targets: Vec<String>,
    pub filter: Option<FilterSpec>,
    pub affected: bool,
    pub parallel: usize,
    pub output: OutputFormat,
}
```

### 2. Config Loader (`config`)

- **Responsibility**: Load and validate workspace configuration
- **Inputs**: Workspace root path
- **Outputs**: Validated `WorkspaceConfig` struct
- **Dependencies**: CLI (for workspace root)

**Key patterns:**
- Search upward for config file (like how Turborepo finds `turbo.json`)
- Support config inheritance (workspace defaults + project overrides)
- Validate early, fail fast with clear errors

**Configuration hierarchy:**
1. Workspace-level: `.aster/config.toml` or `aster.toml`
2. Project-level: `aster.project.toml` (optional overrides)

```rust
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub plugins: Vec<PluginConfig>,
    pub default_targets: Vec<String>,
    pub parallel: usize,
    pub cache: CacheConfig,
}
```

### 3. Language Plugin Registry (`plugins`)

- **Responsibility**: Manage language-specific project parsers
- **Inputs**: Plugin configurations
- **Outputs**: Registered plugin instances that implement `LanguagePlugin` trait
- **Dependencies**: Config Loader

**Key patterns from moon's tier system:**
- Define a trait for language plugins
- Support built-in plugins (Elixir, Node, Python) compiled in
- Future: support external plugins via subprocess or WASM

**Key design principle:** Plugins own all language-specific logic including:
- Parsing native configs (metadata, dependencies)
- Resolving relative paths to project addresses
- Determining cross-project target dependencies (e.g., `test` depends on `:build` of dependency projects)

```rust
/// Context passed to plugins for target detection
pub struct TargetContext<'a> {
    /// Path to the native config file (e.g., package.json, mix.exs)
    pub config_path: &'a Path,
    /// Project directory (parent of config_path)
    pub project_dir: &'a Path,
    /// Workspace root directory
    pub workspace_root: &'a Path,
    /// Dependencies parsed from native config (raw paths, not yet resolved)
    pub dependencies: &'a [LocalDependency],
}

pub trait LanguagePlugin: Send + Sync {
    /// Plugin identifier (e.g., "nodejs", "elixir")
    fn name(&self) -> &str;

    /// Files that identify this project type
    fn marker_files(&self) -> &[&str];

    /// Parse native config to extract project metadata
    fn parse_project(&self, root: &Path, config_path: &Path) -> Result<ProjectMetadata>;

    /// Extract dependencies from native config (returns raw relative paths)
    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>>;

    /// Detect available targets with their dependencies
    ///
    /// Plugin receives raw context and is responsible for:
    /// - Resolving dependency paths to project addresses
    /// - Determining what cross-project dependencies each target needs
    /// - Returning fully-specified Target.depends_on lists
    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>>;
}
```

**Path resolution flow:**
```
Scanner finds marker file
    ↓
Plugin.parse_dependencies() returns raw paths (e.g., "../../libs/core")
    ↓
Scanner creates TargetContext with raw dependencies
    ↓
Plugin.detect_targets(ctx) resolves paths internally:
    - ctx.project_dir.join(dep.path).canonicalize()
    - strip workspace_root prefix → "//libs/core"
    - Add ":build" suffix for cross-project deps
    ↓
Returns HashMap<target_name, Target { command, depends_on }>
```

**Built-in plugins:**
| Plugin | Marker Files | Parses |
|--------|--------------|--------|
| Elixir | `mix.exs` | deps, project name, version |
| Node | `package.json` | dependencies, scripts |
| Python | `pyproject.toml` | dependencies, project metadata |

### 4. Discovery Engine (`discovery`)

- **Responsibility**: Find all projects in the workspace by scanning for marker files
- **Inputs**: Workspace root, registered plugins
- **Outputs**: List of discovered `Project` instances with metadata
- **Dependencies**: Config Loader, Plugin Registry

**Key patterns:**
- Walk filesystem once, checking each directory against all plugins
- Use parallel directory traversal (rayon or tokio)
- Respect `.gitignore` and custom ignore patterns
- Cache discovery results between runs

```rust
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub language: Language,
    pub metadata: ProjectMetadata,
    pub dependencies: Vec<Dependency>,
    pub targets: Vec<Target>,
}
```

**Performance considerations:**
- Use `ignore` crate for gitignore-aware walking
- Parallel file parsing after discovery
- Cache project metadata with file hash invalidation

### 5. Project Graph (`graph`)

- **Responsibility**: Build DAG of project dependencies
- **Inputs**: List of discovered projects
- **Outputs**: `ProjectGraph` (DAG structure)
- **Dependencies**: Discovery Engine

**Key patterns from Nx/Turborepo:**
- Distinguish workspace dependencies (internal) from external (npm, hex, pip)
- Resolve dependency names to project paths
- Detect cycles and report clearly
- Support transitive dependency queries

```rust
pub struct ProjectGraph {
    projects: HashMap<String, ProjectId>,
    edges: HashMap<ProjectId, Vec<ProjectId>>,  // project -> dependencies
    reverse_edges: HashMap<ProjectId, Vec<ProjectId>>,  // project -> dependents
}

impl ProjectGraph {
    /// Get all projects that depend on the given project (transitively)
    pub fn dependents(&self, project: ProjectId) -> Vec<ProjectId>;

    /// Get all dependencies of the given project (transitively)
    pub fn dependencies(&self, project: ProjectId) -> Vec<ProjectId>;

    /// Topological sort of all projects
    pub fn topological_order(&self) -> Result<Vec<ProjectId>, CycleError>;
}
```

**Important:** The project graph and task graph are NOT isomorphic (as Nx documentation emphasizes). Building `app1` may require building `lib`, but testing `app1` does not require testing `lib`.

### 6. Git Analysis / Affected (`affected`)

- **Responsibility**: Determine which projects are affected by recent changes
- **Inputs**: Project graph, git ref range (e.g., `main..HEAD`)
- **Outputs**: Set of affected project IDs
- **Dependencies**: Project Graph, git repository

**Key patterns from dotnet-affected and Nx:**
1. Get list of changed files via `git diff --name-only <base>...<head>`
2. Map changed files to projects (which project owns each file?)
3. Traverse project graph to find all dependents (transitively)
4. Return union of directly changed + transitively affected

```rust
pub struct AffectedAnalyzer {
    project_graph: Arc<ProjectGraph>,
    repo: git2::Repository,
}

impl AffectedAnalyzer {
    /// Find projects affected by changes between two refs
    pub fn affected(&self, base: &str, head: &str) -> Result<HashSet<ProjectId>>;

    /// Find projects affected by uncommitted changes
    pub fn affected_uncommitted(&self) -> Result<HashSet<ProjectId>>;
}
```

**File-to-project mapping:**
- Each project owns files under its directory
- Shared files (workspace root) affect all projects
- Config file changes may affect all projects using that config

### 7. Target Graph (`graph::targets`)

- **Responsibility**: Build DAG of target dependencies for execution planning
- **Inputs**: Discovered projects with resolved targets
- **Outputs**: `TargetGraph` (execution DAG at target level)
- **Dependencies**: Discovery (provides projects with targets)

**Key insight from Nx:**
> "The task graph and project graph aren't isomorphic. For example, even though apps depend on a lib, testing app1 doesn't depend on testing lib."

**Two graph types:**
1. **ProjectGraph** - Project-level dependencies (//project → //project)
   - Used for: `affected` analysis, high-level visualization
2. **TargetGraph** - Target-level dependencies (//project:target → //project:target)
   - Used for: execution ordering, `aster graph`, `aster why`

Target dependencies are already resolved by plugins:
- `//services/api:test` → `[//services/api:deps, //libs/core:build]`
- `//services/api:build` → `[//services/api:deps, //libs/core:build]`
- `//services/api:deps` → `[]`

```rust
pub struct TargetNode {
    /// Full target address (//path/to/project:target)
    pub address: String,
    /// Project address (//path/to/project)
    pub project_address: String,
    /// Target name (test, build, deps, etc.)
    pub target_name: String,
    /// Command to execute
    pub command: String,
}

pub struct TargetGraph {
    graph: DiGraph<TargetNode, ()>,
    index_by_address: HashMap<String, NodeIndex>,
}

impl TargetGraph {
    /// Get direct dependencies of a target
    pub fn dependencies(&self, address: &str) -> Vec<&TargetNode>;

    /// Find shortest dependency path between two targets
    pub fn find_path(&self, from: &str, to: &str) -> Option<Vec<String>>;

    /// Check for cycles
    pub fn find_cycle(&self) -> Option<CycleError>;
}
```

**CLI commands using TargetGraph:**
- `aster graph` - Shows all targets grouped by project with dependencies
- `aster graph //project:target` - Shows specific target's dependencies
- `aster why //a:test //b:deps` - Shows dependency path between targets

### 8. Task Scheduler (`scheduler`)

- **Responsibility**: Orchestrate parallel task execution respecting dependencies
- **Inputs**: Task graph, parallelism limit
- **Outputs**: Stream of task execution events
- **Dependencies**: Task Graph

**Key patterns from build systems:**
- Use Kahn's algorithm for topological scheduling (better for parallel execution than DFS)
- Track in-degree of each task
- When task completes, decrement in-degree of dependents
- Execute tasks with zero in-degree in parallel

```rust
pub struct Scheduler {
    task_graph: TaskGraph,
    parallelism: usize,
    completed: HashSet<TaskId>,
    in_progress: HashSet<TaskId>,
}

impl Scheduler {
    /// Get next batch of tasks to execute
    pub fn next_batch(&mut self) -> Vec<Task>;

    /// Mark task as complete, returns newly unblocked tasks
    pub fn complete(&mut self, task_id: TaskId) -> Vec<Task>;

    /// Check if all tasks are complete
    pub fn is_done(&self) -> bool;
}
```

**Parallelism model:**
- Respect `--parallel N` flag
- Default to number of CPU cores
- Allow unlimited with `--parallel 0`

### 9. Task Runner (`runner`)

- **Responsibility**: Execute individual tasks as subprocesses
- **Inputs**: Task definition, working directory
- **Outputs**: Task result (exit code, stdout, stderr, duration)
- **Dependencies**: Scheduler (drives execution)

**Key patterns:**
- Spawn subprocess with correct working directory
- Capture stdout/stderr (streaming or buffered)
- Handle timeouts and signals
- Support environment variable injection

```rust
pub struct TaskRunner {
    // Configuration
}

impl TaskRunner {
    pub async fn run(&self, task: &Task) -> Result<TaskResult>;
}

pub struct TaskResult {
    pub task_id: TaskId,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub cached: bool,
}
```

### 10. Output Handler (`output`)

- **Responsibility**: Present results to user (terminal UI or structured output)
- **Inputs**: Stream of task events (started, progress, completed, failed)
- **Outputs**: Terminal display or JSON
- **Dependencies**: Runner (provides events)

**Two modes:**

1. **Interactive TUI** (default for TTY):
   - Progress bars for running tasks
   - Spinners for pending tasks
   - Live output streaming
   - Summary on completion

2. **JSON output** (for CI/scripting):
   - Structured events
   - Machine-parseable results
   - No ANSI codes

**Recommended crates:**
- `indicatif` for progress bars and spinners
- `console` for terminal detection and styling
- `serde_json` for JSON output

```rust
pub enum OutputMode {
    Interactive,
    Json,
    Quiet,
}

pub trait OutputHandler: Send {
    fn task_started(&mut self, task: &Task);
    fn task_output(&mut self, task_id: TaskId, line: &str);
    fn task_completed(&mut self, result: &TaskResult);
    fn summary(&mut self, results: &[TaskResult]);
}
```

## Data Flow

1. **Startup**: CLI parses args, finds workspace root
2. **Plugin Registration**: Register built-in language plugins
3. **Discovery**: Walk filesystem using `ignore` crate, find marker files
4. **Project Parsing**: For each marker file:
   - Plugin parses metadata (`parse_project`)
   - Plugin parses raw dependencies (`parse_dependencies`)
   - Scanner creates `TargetContext` with raw paths
   - Plugin detects targets with resolved dependencies (`detect_targets`)
   - TargetResolver merges with aster.toml overrides, resolves `//self:`
5. **Graph Building**:
   - `ProjectGraph`: project-level DAG from dependencies
   - `TargetGraph`: target-level DAG from `Target.depends_on`
6. **Affected Analysis** (if `--affected`): Git diff → changed files → affected projects
7. **Execution**: Execute targets in DAG order (deps before dependents)
8. **Output**: Stream results to terminal

```
                              ┌─────────────────┐
                              │  CLI Entry      │
                              └────────┬────────┘
                                       │
                              ┌────────▼────────┐
                              │  Plugin         │
                              │  Registry       │
                              └────────┬────────┘
                                       │
                              ┌────────▼────────┐
                              │  Scanner        │◄──── Finds marker files
                              └────────┬────────┘
                                       │
                    ┌──────────────────┼──────────────────┐
                    ▼                  ▼                  ▼
             ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
             │ NodeJsPlugin│   │ElixirPlugin │   │PythonPlugin │
             │             │   │             │   │             │
             │ parse_*     │   │ parse_*     │   │ parse_*     │
             │ detect_*    │   │ detect_*    │   │ detect_*    │
             └──────┬──────┘   └──────┬──────┘   └──────┬──────┘
                    │                 │                 │
                    └─────────────────┼─────────────────┘
                                      ▼
                              ┌───────────────┐
                              │TargetResolver │◄──── Merges aster.toml
                              │ (//self: → //)│      overrides
                              └───────┬───────┘
                                      │
                    ┌─────────────────┴─────────────────┐
                    ▼                                   ▼
             ┌─────────────┐                    ┌─────────────┐
             │ProjectGraph │                    │ TargetGraph │
             │ (projects)  │                    │ (targets)   │
             └──────┬──────┘                    └──────┬──────┘
                    │                                  │
                    ▼                                  ▼
             ┌─────────────┐                    ┌─────────────┐
             │  Affected   │                    │  Executor   │
             │  Analysis   │                    │ (DAG order) │
             └─────────────┘                    └─────────────┘
```

## Build Order

Recommended implementation order based on dependencies:

### Phase 1: Foundation

1. **CLI Entry** — No dependencies, establishes command structure
   - Define all commands and argument types
   - Use clap with derive
   - ~200-300 LOC

2. **Config Loader** — Depends on CLI for root path
   - TOML parsing with serde
   - Config validation
   - ~150-200 LOC

3. **Plugin Trait** — No dependencies, defines contract
   - `LanguagePlugin` trait definition
   - Common types (`ProjectMetadata`, `Dependency`)
   - ~100-150 LOC

### Phase 2: Core Engine

4. **Language Plugins** — Depends on Plugin Trait
   - Implement one plugin at a time (suggest: Elixir first)
   - Each plugin: ~200-400 LOC depending on complexity

5. **Discovery Engine** — Depends on Config, Plugins
   - Filesystem traversal
   - Plugin matching
   - ~200-300 LOC

6. **Project Graph** — Depends on Discovery
   - DAG construction
   - Cycle detection
   - Transitive queries
   - ~300-400 LOC

### Phase 3: Execution Engine

7. **Task Graph** — Depends on Project Graph
   - Target-to-task mapping
   - Task dependency resolution
   - ~200-300 LOC

8. **Scheduler** — Depends on Task Graph
   - Kahn's algorithm
   - Parallel batch selection
   - ~200-250 LOC

9. **Task Runner** — Depends on Scheduler
   - Subprocess management
   - Output capture
   - ~250-350 LOC

### Phase 4: User Interface

10. **Output Handler** — Depends on Runner events
    - Terminal UI (indicatif)
    - JSON output
    - ~300-400 LOC

### Phase 5: Advanced Features

11. **Affected Analysis** — Depends on Project Graph
    - Git integration
    - File-to-project mapping
    - Transitive impact
    - ~300-400 LOC

12. **Caching** (future) — Depends on Task Runner
    - Hash computation
    - Cache storage/retrieval
    - ~400-500 LOC

## Extension Points

### Language Plugin System

Primary extension point. New languages added by implementing `LanguagePlugin`:

```rust
pub trait LanguagePlugin: Send + Sync {
    fn name(&self) -> &str;
    fn marker_files(&self) -> &[&str];
    fn parse_project(&self, root: &Path, config_path: &Path) -> Result<ProjectMetadata>;
    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>>;
    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>>;
}
```

**Plugin responsibilities:**
- Parse native config files for metadata and dependencies
- Resolve relative dependency paths to workspace addresses
- Determine cross-project target dependencies (e.g., `:test` depends on dependency's `:build`)
- Map standard targets to native commands

**Initial built-in plugins:**
- Elixir (`mix.exs`) - `mix test`, `mix compile`, `mix deps.get`, `mix credo`
- Node (`package.json`) - maps npm scripts to targets
- Python (`pyproject.toml`) - `pytest`, `python -m build`, `ruff`, `poetry install`

**Future extension approaches:**
1. **Compiled-in**: Add more plugins to core binary
2. **Subprocess**: External binaries following a protocol (like cargo's approach)
3. **WASM**: Sandboxed plugins for security (like Zellij)

### Output Formatters

Allow custom output formats:

```rust
pub trait OutputFormatter: Send {
    fn format_event(&mut self, event: &Event) -> Option<String>;
    fn format_summary(&self, results: &[TaskResult]) -> String;
}
```

Built-in formatters:
- `TerminalFormatter` (default, interactive)
- `JsonFormatter` (structured output)
- `QuietFormatter` (errors only)

### Target Definitions

Allow projects to define custom targets beyond language defaults:

```toml
# aster.project.toml
[targets.custom]
command = "make deploy"
depends_on = ["build"]
```

## Anti-Patterns to Avoid

### 1. Coupling Project Graph and Task Graph

**Wrong:** Assume building `app` requires building all of `app`'s dependencies' tests.

**Right:** Task dependencies are separate from project dependencies. Only `build` typically has cross-project dependencies.

### 2. Synchronous Discovery

**Wrong:** Parse each project file sequentially.

**Right:** Discover files in parallel, then parse in parallel with rayon.

### 3. Blocking on Single Task

**Wrong:** Wait for one task to complete before starting others.

**Right:** Maintain queue of ready tasks, execute up to parallelism limit concurrently.

### 4. Global Mutable State

**Wrong:** Store project graph in global static.

**Right:** Pass graph as `Arc<ProjectGraph>` to components that need it.

### 5. Hardcoded Language Support

**Wrong:** Switch statements on language type throughout codebase.

**Right:** All language-specific logic in plugin implementations behind trait.

## Scalability Considerations

| Concern | 10 Projects | 100 Projects | 1000 Projects |
|---------|-------------|--------------|---------------|
| Discovery | Sequential OK | Parallel walking | Parallel + caching |
| Graph Build | In-memory | In-memory | Incremental/cached |
| Scheduling | Simple queue | Batched parallel | Distributed (future) |
| Output | Stream all | Summarize | Aggregate + filter |

## Sources

- [Turborepo CLI Architecture](https://deepwiki.com/vercel/turborepo/2.4-cli-architecture) - Detailed component breakdown
- [Nx Mental Model](https://nx.dev/docs/concepts/mental-model) - Project graph vs task graph distinction
- [Buck2 Architecture](https://engineering.fb.com/2023/04/06/open-source/buck2-open-source-large-scale-build-system/) - Single incremental dependency graph
- [Bazel Dependencies](https://bazel.build/concepts/dependencies) - Build graph concepts
- [moon Build System](https://moonrepo.dev/moon) - Rust-based monorepo tool
- [DAG Scheduling Patterns](https://brunoscheufler.com/blog/2021-11-27-scheduling-tasks-with-topological-sorting) - Kahn's algorithm for task scheduling
- [Rust Plugin Systems](https://www.arroyo.dev/blog/rust-plugin-systems/) - Extension approaches
- [indicatif](https://github.com/console-rs/indicatif) - Progress bar library for Rust
