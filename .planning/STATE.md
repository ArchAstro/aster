# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-01-22)

**Core value:** Auto-detect the dependency graph from native configs. If the tool can't figure it out from mix.exs/package.json/pyproject.toml, something is wrong.
**Current focus:** Phase 3 - CLI & Git

## Current Position

Phase: 3 of 4 (CLI & Git)
Plan: 1 of 2 in current phase
Status: In progress
Last activity: 2026-01-22 - Completed 03-01-PLAN.md (Target Execution Engine)

Progress: [██████░░░░] 60%

## Performance Metrics

**Velocity:**
- Total plans completed: 6
- Average duration: 4m 18s
- Total execution time: 0.43 hours

**By Phase:**

| Phase | Plans | Total | Avg/Plan |
|-------|-------|-------|----------|
| 01-foundation | 3 | 13m 7s | 4m 22s |
| 02-language-plugins | 2 | 6m 26s | 3m 13s |
| 03-cli-git | 1 | 6m 33s | 6m 33s |

**Recent Trend:**
- Last 5 plans: 01-03 (6m 41s), 02-01 (3m 12s), 02-02 (3m 14s), 03-01 (6m 33s)
- Trend: Consistent velocity

*Updated after each plan completion*

## Accumulated Context

### Decisions

Decisions are logged in PROJECT.md Key Decisions table.
Recent decisions affecting current work:

**Phase 1 (Foundation):**
- aster.toml takes priority over .git for workspace detection
- Address path stores literal string including /... for recursive globs
- LanguagePlugin trait requires Send + Sync for thread safety
- depends_on addresses stored as LocalDependency with address string as path
- Name collisions resolved by appending plugin name suffix (e.g., core-nodejs)
- Edge direction dependent->dependency requires reverse of toposort result
- Absolute file: paths normalized via workspace root derivation
- Target suffix stripped from addresses (//pkg:build -> //pkg)

**Phase 2 (Language Plugins):**
- Elixir: Regex extraction via std::sync::LazyLock for compiled patterns
- Elixir: in_umbrella:true resolves to ../name (implicit path)
- Elixir: Whitespace normalization handles multiline dependencies
- Python: PEP 621 project.name takes priority over tool.poetry.name
- Python: Serde untagged enum for Poetry dependency variants (Version/Table)
- Both plugins implement Send + Sync per LanguagePlugin trait requirement
- Target merge strategy: Custom targets override at key level, not wholesale replacement
- CLI plugin registration: All three plugins registered by default

**Phase 3 (CLI & Git):**
- DAG level computation for parallel execution (level N runs after level N-1 completes)
- External subcommand for targets (`aster test`, `aster build` without predefined list)
- Continue on failure (see all failures, not just first one)
- Skip missing targets gracefully (not an error if project doesn't define target)
- A* with uniform cost for path finding (Dijkstra behavior)

### Pending Todos

None.

### Blockers/Concerns

None yet.

## Session Continuity

Last session: 2026-01-22
Stopped at: Completed 03-01-PLAN.md (Target Execution Engine)
Resume file: None
