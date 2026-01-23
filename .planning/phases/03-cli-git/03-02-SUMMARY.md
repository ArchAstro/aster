---
phase: 03-cli-git
plan: 02
subsystem: cli
tags: [git, git2, affected, change-detection, ci-optimization]

# Dependency graph
requires:
  - phase: 03-01
    provides: Executor, ProjectGraph.dependents(), topological_order_subset()
provides:
  - Git change detection with git2 (refs, uncommitted)
  - File-to-project ownership mapping
  - Affected command with --base, --head, --dependents flags
  - Transitive dependent expansion via BFS
affects: [04-polish, ci-integration]

# Tech tracking
tech-stack:
  added: [git2]
  patterns: [BFS for transitive graph traversal]

key-files:
  created:
    - src/git/mod.rs
    - src/git/affected.rs
    - src/git/file_owner.rs
  modified:
    - src/cli/commands.rs
    - src/main.rs
    - src/lib.rs
    - Cargo.toml
    - tests/integration.rs

key-decisions:
  - "git2 for native git integration (no subprocess spawning)"
  - "Uncommitted changes included by default (Nx-style behavior)"
  - "Head ref omission triggers base..HEAD plus uncommitted"
  - "Longest path match for nested project ownership"

patterns-established:
  - "BFS for transitive dependent expansion"
  - "Path prefix matching for file ownership"

# Metrics
duration: 5m 49s
completed: 2026-01-22
---

# Phase 3 Plan 2: Git-Aware Affected Detection Summary

**git2-powered change detection between refs with file-to-project mapping and transitive dependent expansion via --dependents flag**

## Performance

- **Duration:** 5m 49s
- **Started:** 2026-01-22T23:57:14Z
- **Completed:** 2026-01-23T00:03:03Z
- **Tasks:** 3
- **Files modified:** 9

## Accomplishments
- Git change detection using git2 (diff_tree_to_tree for refs, statuses for uncommitted)
- File-to-project ownership mapping with longest-path-first matching
- `aster affected <target> --base=ref --head=ref --dependents` command
- Integration tests covering real git repositories

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement git change detection with git2** - `78ccc7d` (feat)
2. **Task 2: Implement file-to-project mapping** - `276958e` (feat)
3. **Task 3: Wire affected command into CLI** - `3527c59` (feat)

## Files Created/Modified

- `src/git/mod.rs` - Module exports for git integration
- `src/git/affected.rs` - AffectedDetector with changed_files_between_refs, uncommitted_changes, all_affected_files
- `src/git/file_owner.rs` - files_to_projects and affected_with_dependents functions
- `src/cli/commands.rs` - Added Affected subcommand with --base, --head, --dependents args
- `src/main.rs` - Handle Affected command with full execution flow
- `src/lib.rs` - Export git module and AffectedDetector
- `Cargo.toml` - Added git2 = "0.19" dependency
- `tests/integration.rs` - Added 6 integration tests for affected command

## Decisions Made

| Decision | Rationale |
|----------|-----------|
| git2 for native git integration | No subprocess spawning, cross-platform, rich API |
| Uncommitted included by default | Matches Nx behavior, useful for local development |
| Head ref omission = base..HEAD + uncommitted | Intuitive default: "what changed since main?" |
| Longest path match for ownership | Correctly handles nested projects (services/api/submodule vs services/api) |
| BFS for dependent expansion | Simple, correct, handles diamond dependencies |

## Deviations from Plan

None - plan executed exactly as written.

## Test Coverage

- 119 unit tests passing (10 new git tests)
- 20 integration tests passing (6 new affected tests)
- New tests added for:
  - AffectedDetector (9 tests: refs, uncommitted, errors)
  - file_owner (10 tests: mapping, dependents, edge cases)
  - CLI integration (6 tests: real git repos, flags)

## Verification Results

All verification criteria met:
- `cargo test` - 139 tests pass
- `cargo build --release` - builds successfully
- git2 integrated, Repository opened from workspace
- Changed files detected between refs (diff_tree_to_tree)
- Uncommitted changes detected (statuses)
- Files mapped to owning projects correctly
- Affected command runs targets on detected projects
- --base and --head flags work as expected
- --dependents flag expands to include transitive dependents
- Clear error when not in git repo

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

Phase 3 (CLI & Git) is now complete:
- Target execution engine with parallel DAG-level execution
- Run command with project selection and scope flags
- Why command for path finding
- Init command for workspace creation
- Affected command for git-aware execution

Ready to proceed with Phase 4 (Polish):
- All core functionality implemented
- CLI is feature-complete for MVP
- Comprehensive test coverage in place

---
*Phase: 03-cli-git*
*Completed: 2026-01-22*
