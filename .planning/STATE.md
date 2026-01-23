# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-01-22)

**Core value:** Auto-detect the dependency graph from native configs. If the tool can't figure it out from mix.exs/package.json/pyproject.toml, something is wrong.
**Current focus:** Phase 4 - Output & Testing (Complete)

## Current Position

Phase: 4 of 4 (Output & Testing)
Plan: 2 of 2 in current phase
Status: Phase 4 complete
Last activity: 2026-01-23 - Completed 04-02-PLAN.md (Progress Display & Log Storage)

Progress: [██████████] 100%

## Performance Metrics

**Velocity:**
- Total plans completed: 9
- Average duration: 4m 38s
- Total execution time: 0.70 hours

**By Phase:**

| Phase | Plans | Total | Avg/Plan |
|-------|-------|-------|----------|
| 01-foundation | 3 | 13m 7s | 4m 22s |
| 02-language-plugins | 2 | 6m 26s | 3m 13s |
| 03-cli-git | 2 | 12m 22s | 6m 11s |
| 04-output-testing | 2 | 11m 59s | 6m 00s |

**Recent Trend:**
- Last 5 plans: 03-01 (6m 33s), 03-02 (5m 49s), 04-01 (7m 47s), 04-02 (4m 12s)
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
- git2 for native git integration (no subprocess spawning)
- Uncommitted changes included by default (Nx-style behavior)
- Head ref omission = base..HEAD + uncommitted
- Longest path match for nested project ownership
- BFS for transitive dependent expansion

**Phase 4 (Output & Testing):**
- JSON output goes to stdout, all diagnostics to stderr
- Global flags (--json, --quiet, --verbose) must precede external subcommand
- Terminal detection for pretty vs compact JSON output
- OutputMode enum for consistent output handling
- Progress spinners write to stderr (keeps stdout clean)
- Spinners only when stderr is a terminal (no spinners for CI/piped output)
- Show last 15 lines of failure output inline
- Log storage at .aster/logs/latest.json

### Pending Todos

None.

### Blockers/Concerns

None.

## Session Continuity

Last session: 2026-01-23
Stopped at: Completed 04-02-PLAN.md (Progress Display & Log Storage)
Resume file: None
