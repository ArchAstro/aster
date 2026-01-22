# Aster

## What This Is

A Rust CLI that computes build dependency graphs for polyglot monorepos. Unlike Nx/Bazel/Buck, aster understands native build tool configs (mix.exs, package.json, pyproject.toml) and auto-generates the DAG — zero config for standard setups, override only when needed. Built for speed (ripgrep-style) and simplicity.

## Core Value

Auto-detect the dependency graph from native configs. If the tool can't figure it out from mix.exs/package.json/pyproject.toml, something is wrong.

## Requirements

### Validated

(None yet — ship to validate)

### Active

- [ ] Parse mix.exs and extract path dependencies to build Elixir DAG
- [ ] Parse package.json and extract file: dependencies to build Node DAG
- [ ] Parse pyproject.toml and extract Poetry dependencies to build Python DAG
- [ ] Bazel/Buck-style project addressing (//path/to/project:target)
- [ ] Auto-discover projects by scanning for config files
- [ ] Standard target mapping (test → mix test, build → npm run build, etc.)
- [ ] Override/extension via aster.toml in project directories
- [ ] CLI: aster <target> [projects...] with --no-deps, --dependents, --all
- [ ] CLI: aster affected <target> with --base/--head for git-aware runs
- [ ] CLI: aster list, aster graph, aster why for introspection
- [ ] All commands support --json for machine output
- [ ] All commands support --verbose, --quiet, --help
- [ ] Nice terminal UI (progress, organized error output)
- [ ] Comprehensive unit test coverage

### Out of Scope

- Caching — deferred to v2
- Parallelism (--jobs) — deferred to v2
- Go plugin — deferred to v2
- Swift plugin — deferred to v2
- Kotlin plugin — deferred to v2
- Cloud features — never (local-first, no vendor lock-in)
- Replacing native build tools — aster orchestrates, doesn't replace

## Context

**Problem:** Nx is going cloud-bait, has complex config, poor output, and doesn't understand Elixir/Mix. Bazel/Buck are heavyweight. None of them actually read native configs to infer the graph.

**Test environment:** ~/archastro/firstlanding-wt9 is a real polyglot monorepo (Elixir, TypeScript, Python, Go) that will be used for integration testing.

**User workflows:**
- Run `aster test` from a project directory → tests that project + dependencies
- Run `aster affected test --base=main` → tests only what changed
- Run `aster graph //services/platform` → visualize what platform depends on

**Project addressing:**
- `//services/platform:test` — absolute path from repo root
- `//src/ts/platform-sdk/examples/nextjs-auth:build` — nested projects are just deeper paths
- `//services/...` — glob for all projects under a path
- Default target when omitted (configurable)

**Plugin behavior:**
- Each plugin knows how to parse its ecosystem's config
- Each plugin maps standard targets to native commands
- Each plugin extracts dependencies to build the graph

## Constraints

- **Language**: Rust — for speed and single-binary distribution
- **Performance**: Must use ignore/grep crates (ripgrep-style) for fast traversal
- **Config**: Convention over configuration — auto-detect first, override second
- **Testing**: Comprehensive unit tests required; integration tests against firstlanding-wt9

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Rust | Speed, single binary, no runtime deps | — Pending |
| Project-local aster.toml for overrides | Keeps config close to code, Bazel-style | — Pending |
| Bazel/Buck path syntax (//path:target) | Familiar to users, unambiguous | — Pending |
| Plugins compiled-in (not dynamic) | Simpler for v1, can revisit later | — Pending |
| No caching in v1 | Keep scope tight, add when core is solid | — Pending |
| No parallelism in v1 | Keep scope tight, add when core is solid | — Pending |

---
*Last updated: 2025-01-22 after initialization*
