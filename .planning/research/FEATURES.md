# Features Research: Build Orchestration Tools

**Domain:** Build dependency graph tools for monorepos
**Researched:** 2026-01-22
**Tools Surveyed:** Nx, Turborepo, Bazel, Pants, Buck2, Rush, Lerna, Moon
**Overall Confidence:** HIGH (cross-verified across multiple authoritative sources)

## Executive Summary

Build orchestration tools cluster around two philosophies: **task runners** (Turborepo, Lage) that focus on speed and minimal configuration, and **build systems** (Bazel, Buck2, Pants) that provide hermetic builds with explicit dependency declarations. Aster's proposed approach -- auto-detecting DAGs from native config files without separate build configuration -- occupies a unique position: the simplicity of task runners with the dependency precision of build systems.

**Key insight:** The market has moved toward dependency inference (Pants pioneered this, reducing BUILD file boilerplate by up to 90%), but no tool yet reads native package manager configs (mix.exs, pyproject.toml, package.json) as the primary source of truth across polyglot repos.

---

## Table Stakes

Features users expect from any build dependency graph tool. Missing these means users won't adopt.

### Dependency Graph Construction

- **What**: Build a directed acyclic graph (DAG) representing project dependencies from source files
- **Why table stakes**: This is the core value proposition. Without an accurate dependency graph, affected detection, build ordering, and caching all fail.
- **Complexity**: Medium
- **Dependencies**: Config file parsing for each supported language
- **What competitors do**:
  - Bazel: Explicit BUILD files (high maintenance burden)
  - Pants: Dependency inference from imports + minimal BUILD files
  - Nx/Turborepo: Read package.json for JS/TS workspaces
  - Buck2: Starlark-based BUCK files

**Sources:**
- [Bazel Dependencies](https://bazel.build/concepts/dependencies)
- [Pants Dependency Inference](https://www.pantsbuild.org/blog/2022/10/27/why-dependency-inference)

---

### Affected/Changed Project Detection

- **What**: Given a git diff (base..head), determine which projects are affected by the changes
- **Why table stakes**: This is the primary use case for CI/CD optimization. "Only test what changed" is the killer feature.
- **Complexity**: Low-Medium
- **Dependencies**: Dependency graph construction, git integration
- **Implementation notes**:
  - Map changed files to projects
  - Traverse dependency graph to find all affected downstream projects
  - Support customizable base branch (main, develop, etc.)

**What competitors do:**
- Nx: `nx affected --target=build` (mature, well-documented)
- Turborepo: Uses `--filter` with git diff
- Pants: Built-in affected detection
- Rush: `git change` project selector

**Sources:**
- [Nx Affected Command](https://nx.dev/docs/features/ci-features/affected)
- [dotnet-affected](https://github.com/leonardochaia/dotnet-affected)

---

### Topological Sort / Build Ordering

- **What**: Determine correct build order respecting dependencies (build B before A if A depends on B)
- **Why table stakes**: Without this, parallel builds break and artifacts are missing
- **Complexity**: Low (Kahn's algorithm or DFS)
- **Dependencies**: Dependency graph construction
- **Implementation notes**:
  - Standard algorithm: Kahn's (BFS) or DFS-based
  - Must detect and report cycles
  - Return ordered list or grouped levels for parallelization

**Sources:**
- [Topological Sorting Wikipedia](https://en.wikipedia.org/wiki/Topological_sorting)

---

### Cycle Detection

- **What**: Detect circular dependencies in the project graph
- **Why table stakes**: Cycles make build ordering impossible. Must fail fast with clear error message.
- **Complexity**: Low (part of topological sort)
- **Dependencies**: Dependency graph construction
- **Implementation notes**:
  - Detect during graph construction or sort
  - Report which projects form the cycle
  - Turborepo 2.4+ reports the specific edges to break

**Sources:**
- [Turborepo 2.4](https://turborepo.com/blog/turbo-2-4)
- [Modular Circular Dependencies](https://modular.js.org/concepts/circular-dependencies/)

---

### JSON/Structured Output

- **What**: Export dependency graph and affected projects as JSON for CI/CD integration
- **Why table stakes**: CI pipelines need machine-readable output to make decisions
- **Complexity**: Low
- **Dependencies**: Dependency graph construction
- **Implementation notes**:
  - `--output json` flag
  - Include: project names, paths, dependencies, affected status
  - Nx: `nx show projects --json --affected`
  - Graph can be cached at `.nx/cache/project-graph.json`

**Sources:**
- [Nx Programmatic API](https://nx.dev/docs/guides/nx-release/programmatic-api)

---

### CLI with Clear Error Messages

- **What**: User-friendly command-line interface with helpful error messages
- **Why table stakes**: Developer experience drives adoption. Cryptic errors kill tools.
- **Complexity**: Medium
- **Dependencies**: None
- **Implementation notes**:
  - Clear subcommands: `aster graph`, `aster affected`, `aster list`
  - Actionable error messages: "Cycle detected: A -> B -> C -> A. Break the cycle by removing one dependency."
  - `--help` with examples

---

## Differentiators

Features that could set Aster apart from competitors.

### Zero-Config Dependency Detection from Native Files (PRIMARY DIFFERENTIATOR)

- **What**: Auto-detect project dependencies by parsing native config files (mix.exs, pyproject.toml, package.json, Cargo.toml, go.mod) without requiring separate BUILD files
- **Why differentiating**:
  - Bazel/Buck2 require extensive BUILD file maintenance (6-24 month migration timelines reported)
  - Pants has dependency inference but still needs BUILD files for target declaration
  - Nx/Turborepo only work well for JS/TS ecosystems
  - **No tool reads native polyglot configs as primary source of truth**
- **Complexity**: High (must parse multiple config formats correctly)
- **Competitive advantage**: "Add aster to any polyglot monorepo in 5 minutes, not 5 months"

**Sources:**
- [Pants vs Bazel](https://www.pantsbuild.org/blog/2021/11/18/pants-vs-bazel) - "Bazel requires a huge amount of handwritten BUILD boilerplate"
- [When to use Bazel](https://earthly.dev/blog/bazel-build/) - "6-24 month timelines for large codebases"

---

### True Polyglot Support

- **What**: First-class support for Elixir (mix.exs), Python (pyproject.toml), JavaScript (package.json), and more -- treating all equally
- **Why differentiating**:
  - Nx/Turborepo: JS/TS-centric
  - Pants: Strong Python/Java/Go, but Elixir support limited
  - Bazel: Requires custom rules per language
  - **Elixir monorepos are underserved** -- no major tool has native mix.exs support
- **Complexity**: High (language-specific parsing)
- **Note**: Start with 2-3 languages, expand based on demand

---

### Simplicity by Design

- **What**: Do one thing well -- compute the dependency graph. Don't be a task runner, build executor, or CI platform.
- **Why differentiating**:
  - Nx is becoming a "build intelligence platform" with AI, self-healing CI, enterprise analytics
  - Bazel is "industrial-strength" but complex
  - **Users frustrated by tool complexity** -- steep learning curves are cited as major adoption barrier
- **Complexity**: N/A (it's about what NOT to build)
- **Competitive advantage**: "aster gives you the graph. You decide what to do with it."

**Sources:**
- [Monorepo Tools Comparison](https://graphite.com/guides/monorepo-tools-a-comprehensive-comparison)

---

### Fast Rust Implementation

- **What**: Written in Rust for speed, single static binary distribution
- **Why differentiating**:
  - Turborepo and Buck2 moved to Rust for performance
  - Nx is porting core to Rust (announced 2025)
  - Single binary = easy installation, no runtime dependencies
- **Complexity**: Already decided (Rust)
- **Competitive advantage**: Fast startup, low memory, easy distribution

**Sources:**
- [Nx 2025 Roadmap](https://github.com/nrwl/nx/discussions/28731) - "port the rest of Nx core into Rust"

---

### Dependency Graph Visualization

- **What**: Generate visual representation of the dependency graph (Graphviz DOT, SVG, or ASCII)
- **Why differentiating**: Helps users understand their monorepo structure
- **Complexity**: Low-Medium
- **Dependencies**: Dependency graph construction
- **Implementation notes**:
  - `--graph` flag outputs DOT format
  - Can pipe to Graphviz: `aster graph --format dot | dot -Tsvg > graph.svg`
  - ASCII fallback for terminal viewing
  - Nx and Turborepo both have this; Pants exports JSON for external tools

**Sources:**
- [Turborepo Package and Task Graphs](https://turbo.build/repo/docs/core-concepts/package-and-task-graph)
- [Nx Explore Graph](https://nx.dev/docs/features/explore-graph)

---

### Explicit Dependency Overrides

- **What**: Allow users to declare dependencies that can't be auto-detected (runtime deps, implicit deps)
- **Why differentiating**: Auto-detection can't catch everything (e.g., runtime-loaded plugins)
- **Complexity**: Low
- **Dependencies**: Config file format decision
- **Implementation notes**:
  - Simple override file: `.aster.toml` or similar
  - Pants approach: "Best practice is to use dependency inference, but override with explicit dependencies when needed"

**Sources:**
- [Pants Third-party Dependencies](https://www.pantsbuild.org/dev/docs/python/overview/third-party-dependencies)

---

## Anti-Features

Things other tools do that Aster should deliberately NOT build. These add complexity without proportional value for Aster's use case.

### Task Execution / Build Running

- **What**: Actually running build commands, test commands, lint commands
- **Why avoid**:
  - Scope creep that leads to reinventing make/just/task
  - Users already have task runners they like
  - Aster's value is the graph, not running commands
- **Who does this**: Nx, Turborepo, Bazel, Pants, Rush (all of them)
- **What to do instead**: Output JSON/structured data that task runners can consume

---

### Local/Remote Caching

- **What**: Caching build outputs and sharing across machines
- **Why avoid**:
  - Complex infrastructure (cache servers, cache invalidation)
  - Tight coupling to build execution
  - Security concerns (cache poisoning)
  - Nx, Turborepo, Bazel all have this -- aster can't compete
- **Who does this**: Nx (Nx Cloud), Turborepo (Vercel Remote Cache), Bazel, Pants
- **What to do instead**: Provide cache keys/hashes that other systems can use

---

### Watch Mode / Incremental Rebuilds

- **What**: Monitoring file system changes and triggering rebuilds
- **Why avoid**:
  - Requires tight integration with build system
  - Platform-specific filesystem watching complexity
  - Users have tools for this (nodemon, watchexec, etc.)
- **Who does this**: Rush, Turborepo, Nx
- **What to do instead**: Fast re-computation of affected projects on demand

---

### Code Generation / Scaffolding

- **What**: Generating new projects, components, boilerplate
- **Why avoid**:
  - Language-specific, framework-specific
  - Huge maintenance burden
  - Not related to dependency graph computation
- **Who does this**: Nx (heavily), Rush
- **What to do instead**: Not at all -- outside scope

---

### CI/CD Integration / Self-Healing CI

- **What**: Deep integration with CI platforms, automatic PR fixes
- **Why avoid**:
  - Requires SaaS/cloud component
  - Competing with well-funded tools (Nx Cloud, Vercel)
  - Vendor lock-in concerns
- **Who does this**: Nx (Self-Healing CI, 60% of fixes auto-committed)
- **What to do instead**: JSON output that works with any CI

---

### Plugin System / Extensibility Framework

- **What**: Supporting third-party plugins, custom rules
- **Why avoid**:
  - Massive API surface to maintain
  - Plugin compatibility hell
  - Bazel's Starlark is powerful but complex
- **Who does this**: Bazel (Starlark), Nx (plugins), Buck2 (BXL)
- **What to do instead**: Well-documented JSON output format; users build their own integrations

---

### Workspace/Package Management

- **What**: Managing package versions, hoisting dependencies, linking
- **Why avoid**:
  - pnpm, yarn, npm workspaces already do this well
  - Language-specific (pip, cargo, mix)
  - Orthogonal to dependency graph computation
- **Who does this**: Lerna, Rush, pnpm workspaces
- **What to do instead**: Read existing workspace configs, don't manage them

---

### Remote Execution / Distributed Builds

- **What**: Running build tasks on remote machines
- **Why avoid**:
  - Complex distributed systems problem
  - Requires infrastructure (Bazel's Remote Execution API)
  - Enterprise feature with enterprise complexity
- **Who does this**: Bazel, Buck2, Pants
- **What to do instead**: Not at all -- requires task execution which we're not doing

---

### AI-Powered Features

- **What**: LLM integration, AI code generation, intelligent suggestions
- **Why avoid**:
  - Rapidly evolving space, high maintenance
  - Not core to dependency graph computation
  - Nx is investing heavily here; can't compete
- **Who does this**: Nx (Claude Code plugin, AI migrations planned for Q1 2026)
- **What to do instead**: Not at all -- outside scope

---

## Feature Dependencies

```
Dependency Graph Construction (CORE)
    |
    +-- Affected Detection (requires graph)
    |
    +-- Topological Sort (requires graph)
    |       |
    |       +-- Cycle Detection (part of sort)
    |
    +-- JSON Output (requires graph)
    |
    +-- Visualization (requires graph)

Config Parsing (per language)
    |
    +-- mix.exs parser --> Graph Construction
    +-- pyproject.toml parser --> Graph Construction
    +-- package.json parser --> Graph Construction
```

---

## MVP Recommendation

For MVP, prioritize:

1. **Dependency Graph Construction** - Core value proposition
2. **Affected Detection** - Primary CI/CD use case
3. **Topological Sort with Cycle Detection** - Required for build ordering
4. **JSON Output** - CI/CD integration
5. **2 Language Parsers** - Start with package.json + one other (pyproject.toml or mix.exs based on target users)

Defer to post-MVP:
- Visualization (nice-to-have, users can pipe JSON to external tools)
- Additional language parsers (add based on user demand)
- Explicit dependency overrides (can manually edit detected graph initially)

---

## Sources

### Primary (HIGH confidence)
- [Nx Documentation](https://nx.dev/)
- [Turborepo Documentation](https://turborepo.dev/docs)
- [Bazel Documentation](https://bazel.build/)
- [Pants Documentation](https://www.pantsbuild.org/)
- [Buck2 Documentation](https://buck2.build/)

### Comparisons (MEDIUM confidence)
- [Monorepo Tools Comparison - Aviator](https://www.aviator.co/blog/monorepo-tools/)
- [Monorepo Explained](https://monorepo.tools/)
- [Graphite Monorepo Tools Comparison](https://graphite.com/guides/monorepo-tools-a-comprehensive-comparison)
- [Pants vs Bazel](https://www.pantsbuild.org/blog/2021/11/18/pants-vs-bazel)

### Ecosystem/Community (MEDIUM confidence)
- [Nx 2025 Roadmap](https://github.com/nrwl/nx/discussions/28731)
- [Common Monorepo Problems - Digma](https://digma.ai/10-common-problems-of-working-with-a-monorepo/)
