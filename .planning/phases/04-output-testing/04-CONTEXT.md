# Phase 4: Output & Testing - Context

**Gathered:** 2026-01-23
**Status:** Ready for planning

<domain>
## Phase Boundary

Polished terminal output with progress indication, JSON machine-readable format for all commands, and validation against a real monorepo. Also includes `aster logs` command to retrieve full output from collapsed runs.

</domain>

<decisions>
## Implementation Decisions

### Progress Display
- Multi-line live status (Nx/Turborepo style)
- Each project shows: header with project + target, color-coded status
- Last 2-3 lines of terminal output beneath each running project
- Completed projects stay visible with final status, stacked above running
- Summary line at bottom: "5/12 complete • 3 running • 4 pending"

### Log Retrieval
- New `aster logs` command for accessing full logs after collapsed runs
- `aster logs` alone: list last run's projects/targets with success/fail indicators
- `aster logs <project:target>`: dump full logs for that specific project/target
- If project wasn't in last run: empty output (not an error)

### Failure Presentation
- Show last 10-15 lines of failure output inline when target fails
- Include hint: "run aster logs //project target for full output"
- End-of-run summary listing all failed projects with exit codes
- Exit code 1 if any target fails (standard CI behavior)

### JSON Output
- `--json` flag outputs final blob only (not streaming)
- Nested by project structure: `{"//services/api": {"test": {"status": "passed", ...}}}`
- Metadata only: status, exit code, duration — use aster logs for actual output
- Non-execution commands use native data structures:
  - `aster list --json` → array of projects
  - `aster graph --json` → adjacency list
  - `aster why --json` → path array

### Claude's Discretion
- Color scheme for status indicators
- Exact formatting of progress display
- `--verbose` and `--quiet` behavior details
- `--help` text formatting
- Integration test structure and assertions
- Log storage location and retention

</decisions>

<specifics>
## Specific Ideas

- Progress display inspired by Nx/Turborepo multi-line output
- Log navigation is progressive: bare command shows what's available, then drill down

</specifics>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope

</deferred>

---

*Phase: 04-output-testing*
*Context gathered: 2026-01-23*
