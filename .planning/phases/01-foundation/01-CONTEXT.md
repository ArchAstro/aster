# Phase 1: Foundation - Context

**Gathered:** 2025-01-22
**Status:** Ready for planning

<domain>
## Phase Boundary

Graph engine, project discovery, config system, and Node.js plugin. Users can discover projects in a monorepo and build a dependency graph from package.json files. Includes `aster list`, `aster graph`, cycle detection, and aster.toml config support.

</domain>

<decisions>
## Implementation Decisions

### Project Discovery
- Honor .gitignore patterns during discovery (skip node_modules, build dirs, etc. automatically)
- No implicit parent/child relationships from directory nesting — relationships come only from declared dependencies in config files
- If a directory has multiple config files (e.g., package.json AND mix.exs), treat as separate projects
- Name projects from their config files; if names collide, add language suffix (e.g., `core-node`, `core-elixir`)

### aster.toml Format
- Simple list syntax for dependencies: `depends_on = ["//services/platform:build"]`
- Simple key-value for targets: `[targets]` with `lint = "npm run eslint"`
- Support root aster.toml at workspace root for global settings
- Project name override: `name = "my-project"` at top level

### Claude's Discretion
- Whether to honor npm/pnpm workspace declarations during discovery
- Root aster.toml scope (exclude patterns, default targets, other global settings)
- Graph visualization format (`aster graph` output style)
- Error presentation verbosity

</decisions>

<specifics>
## Specific Ideas

- Dependencies should be inferred from actual config files (package.json `file:` deps, mix.exs `path:` deps), not assumed from filesystem structure
- Config syntax should be minimal — simple lists and key-values, not nested tables with options

</specifics>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope

</deferred>

---

*Phase: 01-foundation*
*Context gathered: 2025-01-22*
