---
phase: 02
plan: 02
subsystem: targets
tags: [target-resolution, polyglot, npm, mix, pytest, ruff]

dependency-graph:
  requires: [02-01]
  provides: [target-resolver, polyglot-discovery, cli-registration]
  affects: [03-01, 03-02]

tech-stack:
  added: []
  patterns:
    - TargetResolver struct with static defaults_for_plugin
    - Merge-at-key-level for target overrides

key-files:
  created:
    - src/targets/mod.rs
    - src/targets/resolver.rs
  modified:
    - src/discovery/scanner.rs
    - src/lib.rs
    - src/main.rs
    - tests/integration.rs

decisions:
  - id: target-merge-strategy
    choice: Custom targets override at key level, not wholesale replacement
    rationale: User can override test without losing build/lint defaults
  - id: cli-all-plugins
    choice: Register all three plugins (nodejs/elixir/python) in main.rs
    rationale: CLI should discover all supported languages by default

metrics:
  duration: 3m 14s
  tests-added: 12
  tests-total: 92
  completed: 2026-01-22
---

# Phase 02 Plan 02: Target Resolution Summary

Target resolver with per-language defaults (npm/mix/pytest) plus polyglot CLI integration.

## Performance

- **Duration:** 3m 14s
- **Started:** 2026-01-22T21:36:24Z
- **Completed:** 2026-01-22T21:39:38Z
- **Tasks:** 3
- **Files modified:** 6

## Accomplishments

- TargetResolver provides sensible defaults for nodejs/elixir/python
- Custom aster.toml targets merge with defaults at key level
- Discovery populates resolved targets for all projects
- CLI registers all three language plugins
- Integration tests verify polyglot workspace discovery end-to-end

## Task Commits

Each task was committed atomically:

1. **Task 1: Create target resolver module with per-language defaults** - `2b2a702` (feat)
2. **Task 2: Integrate target resolution into project discovery** - `d8a4222` (feat)
3. **Task 3: End-to-end verification with all three plugins** - `4f18b8e` (feat)

## Files Created/Modified

- `src/targets/mod.rs` - Module exports for target resolution
- `src/targets/resolver.rs` - TargetResolver with defaults_for_plugin and resolve()
- `src/discovery/scanner.rs` - Integration with TargetResolver during discovery
- `src/lib.rs` - Added targets module and TargetResolver re-export
- `src/main.rs` - Register ElixirPlugin and PythonPlugin in CLI
- `tests/integration.rs` - 4 new polyglot integration tests

## Decisions Made

- **Target merge strategy:** Custom targets override at key level, not wholesale replacement. This means specifying only `[targets] test = "..."` in aster.toml keeps the default build/lint targets.
- **CLI plugin registration:** All three plugins registered by default so polyglot workspaces work out of the box.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Registered missing plugins in main.rs**
- **Found during:** Task 3 (integration tests)
- **Issue:** main.rs only registered NodeJsPlugin, causing Elixir/Python projects to not be discovered
- **Fix:** Added ElixirPlugin and PythonPlugin registration in main.rs
- **Files modified:** src/main.rs
- **Verification:** All 14 integration tests pass
- **Committed in:** 4f18b8e (Task 3 commit)

---

**Total deviations:** 1 auto-fixed (1 blocking)
**Impact on plan:** Essential for polyglot functionality. The plan assumed plugins would be registered but didn't explicitly state where.

## Issues Encountered

None - tests passed once plugins were registered.

## Test Results

```
running 78 tests (unit)
running 14 tests (integration)
test result: ok. 92 passed; 0 failed
```

New tests added: 12 (7 target resolver + 4 scanner + 1 polyglot integration as 4 tests)

## Success Criteria Verification

- [x] Target resolver returns correct defaults for nodejs/elixir/python
- [x] Custom aster.toml targets merge with (not replace) defaults
- [x] Discovery populates targets for all discovered projects
- [x] Integration tests prove polyglot workspace discovery works end-to-end

## Phase 2 Complete

All Phase 2 success criteria met:

1. **Elixir path dependencies:** Parsed from mix.exs into graph edges
2. **Python Poetry path dependencies:** Parsed from pyproject.toml into graph edges
3. **Standard targets map to native commands:** test/build/lint resolve to npm/mix/pytest commands per language

## Next Phase Readiness

**Phase 3 (Execution Engine):**
- Target resolution provides commands to execute
- All language plugins registered in CLI
- Graph traversal available via build_graph()
- Ready for topological execution order

**Blockers:** None

---
*Phase: 02-language-plugins*
*Completed: 2026-01-22*
