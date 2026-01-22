# Stack Research: Build Orchestration CLI

**Project:** Aster - Build dependency graph tool for polyglot monorepos
**Researched:** 2026-01-22
**Overall Confidence:** HIGH

## Executive Summary

Aster's requirements align well with the established Rust CLI ecosystem. The stack centers on battle-tested crates: `clap` for CLI parsing, `ignore` for ripgrep-style directory traversal, `petgraph` for DAG computation, and `indicatif` for progress bars. The main complexity lies in parsing Elixir config files (mix.exs), which requires either tree-sitter or custom parsing.

---

## Recommended Stack

### Core CLI Framework

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **clap** | 4.5.54 | CLI argument parsing with derive macros | HIGH |
| **anyhow** | 1.x | Application-level error handling | HIGH |

**clap** v4.5.54 (Jan 2026) - The standard for Rust CLI tools. Use the `derive` feature for struct-based argument definitions. Generates help text, validates input, handles subcommands.

```toml
clap = { version = "4.5", features = ["derive"] }
```

**Why clap:** Dominates the Rust CLI space. The derive API eliminates boilerplate. Subcommand support essential for `aster build`, `aster affected`, etc.

**anyhow** - For application code, use `anyhow::Result<T>` throughout. Provides context chaining (`with_context`), backtraces, and clean error display.

```toml
anyhow = "1.0"
```

**Why anyhow over thiserror:** Aster is an application, not a library. Users don't need to match on error variants - they need readable error messages. Use `thiserror` only if you later extract a library crate.

---

### Directory Traversal

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **ignore** | 0.4.25 | Fast parallel directory traversal with gitignore support | HIGH |

**ignore** v0.4.25 - From the ripgrep author (BurntSushi). Provides `WalkBuilder` for configuring traversal, `WalkParallel` for multi-threaded walks. Automatically respects `.gitignore`, `.ignore`, and custom ignore files.

```toml
ignore = "0.4"
```

**Why ignore over walkdir:**
- `ignore` includes `walkdir` internally but adds gitignore parsing
- `WalkParallel` enables ripgrep-style performance with parallel traversal
- In monorepos, respecting ignore files is essential (node_modules, _build, etc.)
- Same author, `ignore` is the "batteries included" version

**Why ignore over jwalk:** `jwalk` provides sorted parallel results, but Aster doesn't need sorting during traversal. `ignore`'s gitignore integration is more valuable than `jwalk`'s sorted output.

---

### Config File Parsing

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **serde** | 1.0.228 | Serialization framework | HIGH |
| **serde_json** | 1.0.149 | JSON parsing (package.json) | HIGH |
| **toml** | 0.8.x | TOML parsing (pyproject.toml, Cargo.toml) | HIGH |
| **tree-sitter** | 0.26.3 | Elixir syntax parsing (mix.exs) | MEDIUM |
| **tree-sitter-elixir** | latest | Elixir grammar for tree-sitter | MEDIUM |

**serde + serde_json** - Standard for JSON. Parse `package.json` into typed structs.

```toml
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

**toml** - Parse `pyproject.toml` and `Cargo.toml`. Uses serde for deserialization.

```toml
toml = "0.8"
```

**tree-sitter + tree-sitter-elixir** - For parsing `mix.exs` files. Elixir configs are executable code, not declarative data. Tree-sitter provides AST access without executing the Elixir code.

```toml
tree-sitter = "0.26"
tree-sitter-elixir = "0.3"  # Verify version on crates.io
```

**Why tree-sitter for Elixir:**
- `mix.exs` is Elixir code, not a data format like JSON/TOML
- Cannot use serde - need AST parsing
- tree-sitter-elixir is maintained by the Elixir core team
- Provides accurate parsing without Elixir runtime dependency

**Alternative for Elixir:** Custom regex-based parser for the subset of mix.exs patterns you need (deps list extraction). Faster to implement, but fragile. Recommend tree-sitter for correctness.

---

### Graph & DAG Computation

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **petgraph** | 0.8.3 | Graph data structures and algorithms | HIGH |

**petgraph** v0.8.3 - The de facto graph library for Rust. 250M+ downloads. Provides:
- `DiGraph` for directed graphs (your dependency DAG)
- `toposort()` for topological ordering
- `has_path_connecting()` for reachability queries
- Cycle detection with `is_cyclic_directed()`

```toml
petgraph = "0.8"
```

**Why petgraph over daggy:** While `daggy` is DAG-specific (wrapper around petgraph), `petgraph` directly has everything Aster needs. `daggy` adds Walker trait convenience but petgraph's `Dfs`, `Bfs` iterators suffice. Fewer dependencies, more community support.

**Key petgraph patterns for Aster:**
```rust
use petgraph::graph::DiGraph;
use petgraph::algo::{toposort, is_cyclic_directed};
use petgraph::visit::Dfs;

let mut graph: DiGraph<Package, ()> = DiGraph::new();
let idx = graph.add_node(package);
graph.add_edge(from_idx, to_idx, ());

// Build order (reverse topological sort)
let sorted = toposort(&graph, None)?;

// Affected packages (reachability from changed nodes)
let mut dfs = Dfs::new(&graph, changed_node);
while let Some(nx) = dfs.next(&graph) {
    affected.insert(nx);
}
```

---

### Terminal UI & Progress

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **indicatif** | 0.17.x | Progress bars and spinners | HIGH |
| **console** | 0.16.1 | Terminal abstraction, colors | HIGH |

**indicatif** v0.17 - Progress reporting for CLI tools. `ProgressBar` for bounded progress, spinners for unbounded operations. `MultiProgress` for parallel task progress.

```toml
indicatif = "0.17"
```

**console** v0.16.1 - Terminal abstraction from the same author as indicatif. Provides colors, terminal size detection, styling.

```toml
console = "0.16"
```

**Why indicatif over ratatui:** Aster is a CLI tool, not a TUI application. `indicatif` is purpose-built for progress reporting. `ratatui` is for full terminal UIs with layouts, widgets, event loops. Overkill for Aster's needs.

**Pattern for parallel builds:**
```rust
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

let multi = MultiProgress::new();
let style = ProgressStyle::default_bar()
    .template("{prefix:.bold} [{bar:40}] {pos}/{len} {msg}")?;

for package in &packages {
    let pb = multi.add(ProgressBar::new(steps));
    pb.set_style(style.clone());
    pb.set_prefix(package.name.clone());
}
```

---

### Git Integration

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **git2** | 0.20.3 | Git operations (affected detection) | HIGH |

**git2** v0.20.3 - Rust bindings to libgit2. Mature, feature-complete. Essential for:
- Detecting changed files since a ref (`git diff --name-only`)
- Getting current branch/commit
- Repository status

```toml
git2 = "0.20"
```

**Why git2 over gix (gitoxide):**
- `git2` is battle-tested, feature-complete for Aster's needs
- `gix` is pure Rust (appealing) but API is still evolving
- `gix` documentation notes "quite far from parity with git2"
- For simple operations (diff, status, rev-parse), `git2` is the safe choice
- Reconsider `gix` in 2027 when it reaches maturity

**Affected detection pattern:**
```rust
use git2::{Repository, DiffOptions};

let repo = Repository::open(".")?;
let head = repo.head()?.peel_to_commit()?;
let base = repo.revparse_single("main")?.peel_to_commit()?;

let diff = repo.diff_tree_to_tree(
    Some(&base.tree()?),
    Some(&head.tree()?),
    Some(&mut DiffOptions::new()),
)?;

let changed_files: Vec<PathBuf> = diff
    .deltas()
    .filter_map(|d| d.new_file().path().map(PathBuf::from))
    .collect();
```

---

### Parallelism

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **rayon** | 1.11.0 | Data parallelism | HIGH |

**rayon** v1.11.0 - Drop-in parallelism via `par_iter()`. Essential for:
- Parallel config file parsing
- Parallel directory traversal (via `ignore`'s `WalkParallel`)
- Parallel package analysis

```toml
rayon = "1.11"
```

**Pattern:**
```rust
use rayon::prelude::*;

let packages: Vec<Package> = config_paths
    .par_iter()
    .map(|path| parse_config(path))
    .collect::<Result<Vec<_>>>()?;
```

---

### Logging & Diagnostics

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **tracing** | 0.1.41 | Structured logging/instrumentation | MEDIUM |
| **tracing-subscriber** | 0.3.x | Log output formatting | MEDIUM |

**tracing** v0.1.41 - Structured, span-based logging. Better than `log` for understanding execution flow. Optional - can start with `env_logger` and migrate later.

```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

**Why MEDIUM confidence:** For a CLI tool, simple `env_logger` may suffice. `tracing` shines for async services and complex debugging. Consider starting with `env_logger`, upgrade to `tracing` if needed.

**Alternative - simpler logging:**
```toml
env_logger = "0.11"
log = "0.4"
```

---

### Output Serialization

| Crate | Version | Purpose | Confidence |
|-------|---------|---------|------------|
| **serde_json** | 1.0.149 | JSON output for machine consumption | HIGH |

Already listed above. Use for `--json` output flag.

---

## Installation Summary

```toml
[dependencies]
# CLI
clap = { version = "4.5", features = ["derive"] }
anyhow = "1.0"

# File traversal
ignore = "0.4"

# Parsing
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"
tree-sitter = "0.26"
tree-sitter-elixir = "0.3"

# Graph
petgraph = "0.8"

# Terminal
indicatif = "0.17"
console = "0.16"

# Git
git2 = "0.20"

# Parallelism
rayon = "1.11"

# Logging (choose one)
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
# OR for simplicity:
# env_logger = "0.11"
# log = "0.4"
```

---

## Avoid

| Crate | Why Not |
|-------|---------|
| **structopt** | Deprecated - merged into clap 4.x derive API |
| **walkdir** | Use `ignore` instead - includes walkdir + gitignore support |
| **daggy** | Unnecessary wrapper; petgraph suffices for Aster's needs |
| **ratatui** | Overkill for CLI progress; use indicatif for progress bars |
| **gix/gitoxide** | API still evolving, not at parity with git2 yet |
| **tokio** | Aster is CPU-bound (parsing, graph), not I/O-bound; rayon suffices |
| **colored** | Use console instead - from indicatif author, better integration |
| **nom/pest** | For Elixir parsing, tree-sitter with official grammar is more robust |

---

## Open Questions

### 1. Elixir Parsing Strategy (MEDIUM priority)

**Question:** Full tree-sitter AST vs. regex-based extraction?

**Trade-offs:**
- Tree-sitter: Correct parsing, handles edge cases, but adds build complexity (C compilation)
- Regex: Fast to implement, but fragile for complex mix.exs files

**Recommendation:** Start with tree-sitter for correctness. If build times become problematic, consider regex fallback for simple cases.

### 2. Async vs Sync Architecture (LOW priority)

**Question:** Should Aster use tokio for async I/O?

**Current answer:** No. Aster is CPU-bound (parsing files, computing graphs). `rayon` for parallelism suffices. File I/O is fast enough synchronously given SSD prevalence.

**Revisit if:** Network-based package registries are added, or remote git operations become common.

### 3. Config File Caching (MEDIUM priority)

**Question:** Should Aster cache parsed config files?

**Options:**
- No caching: Parse fresh each run (simpler)
- SQLite cache: Store parsed configs with mtime checks
- JSON cache: Serialize to `.aster/cache.json`

**Recommendation:** Start without caching. Add if profiling shows config parsing is the bottleneck (unlikely given Aster's scope).

---

## Confidence Assessment

| Area | Confidence | Rationale |
|------|------------|-----------|
| CLI (clap, anyhow) | HIGH | Industry standard, verified current versions |
| Traversal (ignore) | HIGH | From ripgrep author, proven at scale |
| JSON/TOML parsing | HIGH | serde ecosystem is canonical |
| Elixir parsing | MEDIUM | tree-sitter approach is sound but untested at scale |
| Graph (petgraph) | HIGH | 250M+ downloads, well-documented API |
| Terminal (indicatif) | HIGH | Standard for CLI progress, verified current |
| Git (git2) | HIGH | Mature libgit2 bindings, well-suited for needs |
| Parallelism (rayon) | HIGH | Drop-in parallelism, proven performance |

---

## Sources

### Verified via crates.io / docs.rs
- [clap 4.5.54](https://docs.rs/crate/clap/latest) - Current version confirmed
- [petgraph 0.8.3](https://crates.io/crates/petgraph) - Current version, 250M+ downloads
- [ignore 0.4.25](https://crates.io/crates/ignore) - 91M+ downloads
- [git2 0.20.3](https://docs.rs/crate/git2/latest) - Requires libgit2 1.9.0+
- [rayon 1.11.0](https://docs.rs/crate/rayon/latest) - Released 2025-08-12
- [tree-sitter 0.26.3](https://docs.rs/crate/tree-sitter/latest) - Released 2025-12-13
- [indicatif](https://docs.rs/indicatif/latest/indicatif/) - MultiProgress for parallel tasks
- [console 0.16.1](https://crates.io/crates/console) - 145M+ downloads

### Architecture References
- [gitoxide vs git2 comparison](https://github.com/GitoxideLabs/gitoxide) - gix not yet at parity
- [tree-sitter-elixir](https://github.com/elixir-lang/tree-sitter-elixir) - Official Elixir grammar
- [Error handling best practices](https://leapcell.io/blog/choosing-the-right-rust-error-handling-tool) - anyhow for apps, thiserror for libs
