# Roadmap: Aster

## Overview

Aster delivers a zero-config build dependency graph tool for polyglot monorepos in four phases. Phase 1 establishes the core graph engine, project discovery, and configuration system with the first language plugin. Phase 2 completes polyglot support with Elixir and Python plugins. Phase 3 builds the full CLI surface including git-aware affected detection. Phase 4 polishes output formatting, terminal UI, and validates with integration testing against a real monorepo.

## Phases

- [ ] **Phase 1: Foundation** - Graph engine, discovery, config system, Node.js plugin
- [ ] **Phase 2: Language Plugins** - Elixir and Python plugins, target mapping
- [ ] **Phase 3: CLI & Git** - Full command surface, affected detection
- [ ] **Phase 4: Output & Testing** - Terminal UI, JSON output, integration tests

## Phase Details

### Phase 1: Foundation
**Goal**: Users can discover projects and build a dependency graph from Node.js package.json files
**Depends on**: Nothing (first phase)
**Requirements**: GRAPH-01, GRAPH-02, GRAPH-03, DISC-01, DISC-02, DISC-03, DISC-04, EXT-01, EXT-02, EXT-03, EXT-04, PLUG-01, PLUG-05, TEST-01
**Success Criteria** (what must be TRUE):
  1. User can run `aster list` from anywhere in a monorepo and see all discovered projects
  2. User can run `aster graph` and see the dependency DAG (text representation)
  3. If a cycle exists, aster reports the exact cycle path and exits with error
  4. User can place `aster.toml` in a project to override name, add dependencies, or define custom targets
  5. Node.js `file:` dependencies in package.json are correctly parsed into graph edges
**Plans**: 3 plans

Plans:
- [ ] 01-01-PLAN.md — Core infrastructure: Rust project, workspace detection, address parsing, plugin trait
- [ ] 01-02-PLAN.md — Discovery system: Node.js plugin, aster.toml parsing, project scanner
- [ ] 01-03-PLAN.md — Graph engine and CLI: dependency graph, cycle detection, list/graph commands

### Phase 2: Language Plugins
**Goal**: Users can build dependency graphs from Elixir and Python projects alongside Node.js
**Depends on**: Phase 1
**Requirements**: PLUG-02, PLUG-03, PLUG-04
**Success Criteria** (what must be TRUE):
  1. Elixir `path:` dependencies in mix.exs are correctly parsed into graph edges
  2. Python path dependencies in pyproject.toml (Poetry format) are correctly parsed into graph edges
  3. Standard targets (test, build, lint) map to native commands for each language (mix test, npm test, pytest)
**Plans**: TBD

Plans:
- [ ] 02-01: TBD

### Phase 3: CLI & Git
**Goal**: Users can run targets on projects with full CLI options and git-aware affected detection
**Depends on**: Phase 2
**Requirements**: CLI-01, CLI-02, CLI-03, CLI-04, CLI-05, CLI-06, CLI-07, CLI-08, CLI-09, GIT-01, GIT-02
**Success Criteria** (what must be TRUE):
  1. User can run `aster test //services/platform` and it executes tests with dependencies in correct order
  2. User can run `aster affected test --base=main` and only projects changed since main are tested
  3. User can run `aster why //a //b` and see the dependency path between two projects
  4. Flags --no-deps, --dependents, --all work as expected to control execution scope
  5. `aster list` and `aster graph` show project information for introspection
**Plans**: TBD

Plans:
- [ ] 03-01: TBD
- [ ] 03-02: TBD

### Phase 4: Output & Testing
**Goal**: Users get polished terminal output and the tool is validated against a real monorepo
**Depends on**: Phase 3
**Requirements**: OUT-01, OUT-02, OUT-03, OUT-04, OUT-05, OUT-06, TEST-02
**Success Criteria** (what must be TRUE):
  1. All commands support `--json` flag for machine-readable output
  2. Terminal shows progress indication during multi-project runs
  3. Failures are clearly displayed while successful outputs are organized (not noisy)
  4. `--verbose`, `--quiet`, and `--help` flags work on all commands
  5. Integration tests pass against ~/archastro/firstlanding-wt9 monorepo
**Plans**: TBD

Plans:
- [ ] 04-01: TBD

## Progress

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 1. Foundation | 0/3 | Planned | - |
| 2. Language Plugins | 0/1 | Not started | - |
| 3. CLI & Git | 0/2 | Not started | - |
| 4. Output & Testing | 0/1 | Not started | - |

---
*Roadmap created: 2026-01-22*
*Depth: quick (4 phases)*
