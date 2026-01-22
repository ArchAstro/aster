# Requirements: Aster

**Defined:** 2025-01-22
**Core Value:** Auto-detect the dependency graph from native configs. If the tool can't figure it out from mix.exs/package.json/pyproject.toml, something is wrong.

## v1 Requirements

Requirements for initial release. Each maps to roadmap phases.

### Core Graph Engine

- [x] **GRAPH-01**: Build dependency graph from parsed project configs
- [x] **GRAPH-02**: Detect cycles and report clear error with exact cycle path
- [x] **GRAPH-03**: Topological sort for dependency-ordered execution

### Project Discovery

- [x] **DISC-01**: Auto-discover projects by scanning for config files (package.json, mix.exs, pyproject.toml, aster.toml)
- [x] **DISC-02**: Bazel/Buck-style addressing (//path/to/project:target)
- [x] **DISC-03**: Recursive glob support (//services/... matches all projects under path)
- [x] **DISC-04**: Workspace root detection (walk up to find aster.toml or .git)

### Language Plugins

- [x] **PLUG-01**: Node.js plugin parses package.json and extracts file: dependencies
- [x] **PLUG-02**: Elixir plugin parses mix.exs and extracts path: dependencies
- [x] **PLUG-03**: Python plugin parses pyproject.toml and extracts Poetry path dependencies
- [x] **PLUG-04**: Plugins map standard targets (test, build, lint) to native commands
- [x] **PLUG-05**: Plugin trait enables adding new languages

### Git Integration

- [ ] **GIT-01**: Detect affected projects between git refs (uncommitted, base..head)
- [ ] **GIT-02**: Map changed files to owning projects

### CLI Commands

- [ ] **CLI-01**: `aster <target> [projects...]` runs target on specified projects
- [ ] **CLI-02**: `--no-deps` flag skips running dependencies
- [ ] **CLI-03**: `--dependents` flag also runs projects that depend on targets
- [ ] **CLI-04**: `--all` flag runs on entire repository
- [ ] **CLI-05**: `aster affected <target>` runs on projects affected by git changes
- [ ] **CLI-06**: `--base` and `--head` flags for affected ref range
- [ ] **CLI-07**: `aster list` shows all discovered projects
- [ ] **CLI-08**: `aster graph [project]` visualizes the dependency DAG
- [ ] **CLI-09**: `aster why //a //b` shows dependency path between projects

### CLI Output

- [ ] **OUT-01**: `--json` flag outputs machine-readable JSON for all commands
- [ ] **OUT-02**: Terminal UI with progress indication
- [ ] **OUT-03**: Organized error output (hide success, show failures clearly)
- [ ] **OUT-04**: `--verbose` flag streams all output
- [ ] **OUT-05**: `--quiet` flag shows only final pass/fail
- [ ] **OUT-06**: `--help` flag with clear usage for all commands

### Override & Extension

- [x] **EXT-01**: `aster.toml` in project directory for manual configuration
- [x] **EXT-02**: Declare cross-language dependencies in aster.toml
- [x] **EXT-03**: Override project name (instead of inferring from package config)
- [x] **EXT-04**: Define custom targets with arbitrary commands

### Testing & Quality

- [x] **TEST-01**: Comprehensive unit tests for all components
- [ ] **TEST-02**: Integration tests against ~/archastro/firstlanding-wt9

## v2 Requirements

Deferred to future release. Tracked but not in current roadmap.

### Performance

- **PERF-01**: Caching of build outputs
- **PERF-02**: Parallel execution with --jobs flag
- **PERF-03**: Incremental graph updates (vs full rebuild)

### Additional Plugins

- **PLUG-06**: Go plugin (go.mod)
- **PLUG-07**: Swift plugin (Package.swift)
- **PLUG-08**: Kotlin plugin (build.gradle.kts)

### Watch Mode

- **WATCH-01**: File watching for automatic re-runs
- **WATCH-02**: Selective watching based on project

## Out of Scope

Explicitly excluded. Documented to prevent scope creep.

| Feature | Reason |
|---------|--------|
| Cloud features | Local-first philosophy, no vendor lock-in |
| Remote caching | Adds complexity, defer until local caching proven |
| Task execution replacement | Aster orchestrates, native tools execute |
| Build file generation | Convention over configuration - read, don't write |
| CI/CD integration | Users integrate via CLI, no special CI support needed |
| Plugin marketplace | Compiled-in plugins for v1, revisit if demand |

## Traceability

Which phases cover which requirements. Updated during roadmap creation.

| Requirement | Phase | Status |
|-------------|-------|--------|
| GRAPH-01 | Phase 1 | Complete |
| GRAPH-02 | Phase 1 | Complete |
| GRAPH-03 | Phase 1 | Complete |
| DISC-01 | Phase 1 | Complete |
| DISC-02 | Phase 1 | Complete |
| DISC-03 | Phase 1 | Complete |
| DISC-04 | Phase 1 | Complete |
| PLUG-01 | Phase 1 | Complete |
| PLUG-02 | Phase 2 | Complete |
| PLUG-03 | Phase 2 | Complete |
| PLUG-04 | Phase 2 | Complete |
| PLUG-05 | Phase 1 | Complete |
| GIT-01 | Phase 3 | Pending |
| GIT-02 | Phase 3 | Pending |
| CLI-01 | Phase 3 | Pending |
| CLI-02 | Phase 3 | Pending |
| CLI-03 | Phase 3 | Pending |
| CLI-04 | Phase 3 | Pending |
| CLI-05 | Phase 3 | Pending |
| CLI-06 | Phase 3 | Pending |
| CLI-07 | Phase 3 | Pending |
| CLI-08 | Phase 3 | Pending |
| CLI-09 | Phase 3 | Pending |
| OUT-01 | Phase 4 | Pending |
| OUT-02 | Phase 4 | Pending |
| OUT-03 | Phase 4 | Pending |
| OUT-04 | Phase 4 | Pending |
| OUT-05 | Phase 4 | Pending |
| OUT-06 | Phase 4 | Pending |
| EXT-01 | Phase 1 | Complete |
| EXT-02 | Phase 1 | Complete |
| EXT-03 | Phase 1 | Complete |
| EXT-04 | Phase 1 | Complete |
| TEST-01 | Phase 1 | Complete |
| TEST-02 | Phase 4 | Pending |

**Coverage:**
- v1 requirements: 35 total
- Mapped to phases: 35
- Unmapped: 0

---
*Requirements defined: 2025-01-22*
*Last updated: 2026-01-22 after Phase 2 completion*
