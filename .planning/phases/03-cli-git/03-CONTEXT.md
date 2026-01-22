# Phase 3: CLI & Git - Context

**Gathered:** 2026-01-22
**Status:** Ready for planning

<domain>
## Phase Boundary

Users can run targets on projects with full CLI options and git-aware affected detection. This includes:
- `aster <target> //project` — run targets with dependency ordering
- `aster affected <target>` — run on git-changed projects
- `aster why //a //b` — show dependency path
- `aster init` — initialize workspace
- Flags: --no-deps, --dependents, --all, --base, --head

</domain>

<decisions>
## Implementation Decisions

### Command execution
- Run dependencies first in topo order, then target project
- Parallel execution by default (respect dependency order)
- Continue all on failure — collect all failures at the end
- Grouped output — buffer each project's output, show as block when finished

### Affected detection
- Follow Nx conventions for default behavior
- Research Nx's `affected` command semantics during planning
- Support --base and --head flags for ref range

### Flag design
- Follow Nx conventions for flag naming and behavior
- Research Nx's flag patterns (--no-deps, --parallel, etc.)

### aster init
- Creates root aster.toml only (marks workspace root)
- Discovery handles project detection automatically
- Print summary of what was found after scanning

### Claude's Discretion
- Exact parallelism implementation (thread pool, async, etc.)
- Output buffering strategy
- Error message formatting
- `aster why` path-finding algorithm

</decisions>

<specifics>
## Specific Ideas

- "Follow Nx conventions" — Nx is the reference implementation for CLI behavior
- User tested Phase 2 on ~/archastro/firstlanding-wt9 successfully

</specifics>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope

</deferred>

---

*Phase: 03-cli-git*
*Context gathered: 2026-01-22*
