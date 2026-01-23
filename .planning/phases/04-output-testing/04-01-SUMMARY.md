---
phase: 04
plan: 01
subsystem: cli-output
tags: [json, cli, output-modes, serde]

dependency-graph:
  requires: [03-cli-git]
  provides: [json-output, quiet-mode, output-mode-enum]
  affects: []

tech-stack:
  added: [console]
  patterns: [output-mode-enum, json-serialization, terminal-detection]

key-files:
  created:
    - src/cli/output.rs
  modified:
    - Cargo.toml
    - src/cli/commands.rs
    - src/cli/mod.rs
    - src/main.rs
    - src/executor/runner.rs

decisions:
  - id: json-output-stdout
    choice: "JSON goes to stdout, diagnostics to stderr"
    rationale: "Allows piping: aster list --json | jq"
  - id: global-flag-position
    choice: "Global flags must precede external subcommand"
    rationale: "Standard Unix behavior with clap external_subcommand"
  - id: terminal-detection
    choice: "Pretty JSON if terminal, compact if piped"
    rationale: "Human-readable when interactive, machine-friendly when piped"

metrics:
  duration: 7m 47s
  completed: 2026-01-23
---

# Phase 04 Plan 01: Output Modes & JSON Summary

**One-liner:** Global --json/--quiet/--verbose flags with JSON output for all commands using console crate for terminal detection.

## What Was Built

Added comprehensive output mode support to aster CLI:

1. **OutputMode enum** (`src/cli/output.rs`):
   - `Normal` - default text output
   - `Verbose` - text output with `[aster]` prefixed diagnostics
   - `Quiet` - minimal summary only
   - `Json` - machine-readable JSON output

2. **JSON output structures**:
   - `ProjectInfo` - for `list --json`
   - `GraphOutput` - nodes and edges adjacency list for `graph --json`
   - `WhyOutput` - path finding result for `why --json`
   - `ExecutionOutput` - results and summary for `test/build/affected --json`

3. **Global CLI flags**:
   - `--json` - output JSON to stdout
   - `--quiet` - suppress per-project output
   - `--verbose` - show diagnostic output with `[aster]` prefix
   - Verbose and quiet are mutually exclusive

## Key Implementation Details

### Terminal Detection
Uses `console` crate to detect if stdout is a terminal:
- Terminal: pretty-print JSON with indentation
- Pipe/file: compact single-line JSON

### Output Separation
- JSON output goes ONLY to stdout
- All diagnostic output (verbose messages, errors) goes to stderr
- Enables clean piping: `aster list --json | jq .`

### Executor Output Modes
Updated `Executor` to accept output mode:
- `Executor::with_output_mode()` constructor
- Per-project output suppressed in Quiet/Json modes
- Results still collected for JSON serialization

## Commits

| Hash | Type | Description |
|------|------|-------------|
| d3c2ab5 | feat | Add global flags and OutputMode |
| ccf4fbd | feat | Implement JSON output for introspection commands |
| 6e19293 | feat | Implement JSON and quiet output for execution commands |

## Deviations from Plan

None - plan executed exactly as written.

## Verification Results

```bash
# All tests pass
cargo test --lib  # 151 passed

# --help shows all flags
aster --help | grep -E "(json|quiet|verbose)"
# -v, --verbose  Enable verbose output
# -q, --quiet    Suppress per-project output
#     --json     Output in JSON format

# JSON output works
aster list --json | jq 'type'  # "array"
aster graph --json | jq 'has("nodes")'  # true

# Quiet mode shows minimal output
aster --quiet test --all  # "2 passed, 3 failed"

# Verbose uses stderr
aster list --verbose 2>&1 | grep "Workspace root"
# [aster] Workspace root: /path/to/workspace
```

## Usage Notes

**Important:** Global flags (`--json`, `--quiet`, `--verbose`) must be placed BEFORE the subcommand when using external subcommands like `test`, `build`, etc.

```bash
# Correct
aster --json test --all
aster --quiet build //services/api

# Won't work (flags captured by external subcommand)
aster test --all --json
```

This is standard Unix behavior with clap's `external_subcommand` feature.

## Files Changed

| File | Changes |
|------|---------|
| `Cargo.toml` | Added `console = "0.16"` |
| `src/cli/output.rs` | New: OutputMode enum, JSON structs, helpers |
| `src/cli/commands.rs` | Added --json, --quiet flags, output_mode() method |
| `src/cli/mod.rs` | Export output module and types |
| `src/main.rs` | JSON/quiet handling for all commands |
| `src/executor/runner.rs` | with_output_mode(), suppress output in quiet/json |

## Next Steps

Phase 04 Plan 02 will add:
- Colored output with console crate
- Progress indicators for long-running operations
- Enhanced error formatting
