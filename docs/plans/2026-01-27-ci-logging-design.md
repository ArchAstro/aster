# CI Logging Mode Design

## Overview

Add automatic CI-friendly logging for `aster affected`, `aster run`, and `aster <target>` commands. Normal logging updates terminal output in place using spinners and progress bars, which doesn't render well in CI environments. CI mode provides simple line-by-line output with timestamps.

## Activation

CI mode activates automatically when `stderr` is not a TTY:

```rust
let is_ci = !std::io::stderr().is_terminal();
```

This requires zero configuration - it "just works" when running in CI pipelines.

## Output Format

```
[12:34:56] START //apps/web:build
[12:34:56] START //apps/api:build
[12:34:58] PASS //apps/web:build (2.1s)
[12:34:59] FAIL //apps/api:build (3.2s)
[12:34:59] SKIP //apps/other:build
[12:34:59] ✓ //libs/shared:build cached
```

- Timestamps use `HH:MM:SS` format (local time)
- `START` printed when execution begins
- `PASS`/`FAIL`/`SKIP` printed on completion with duration
- Cached items show `✓` with "cached" suffix (consistent with existing behavior)
- ANSI colors preserved (GitHub Actions and most CI systems support them)

## Implementation

### Changes to `src/ui/progress.rs`

Add CI mode to `ProgressDisplay`:

- New field to track CI mode
- Constructor detects TTY status
- When CI mode active:
  - `add_running()` prints `[HH:MM:SS] START {address}` immediately
  - `mark_complete()` prints `[HH:MM:SS] PASS/FAIL {address} ({duration})`
  - `mark_skipped()` prints `[HH:MM:SS] SKIP {address}`
  - `mark_cached()` prints `[HH:MM:SS] ✓ {address} cached`
- No `MultiProgress`, spinners, or status bar in CI mode - direct `eprintln!` calls

### Interaction with Existing Flags

| Flag | Behavior |
|------|----------|
| `--json` | CI logging disabled (JSON output only) |
| `--quiet` | CI logging disabled (minimal output) |
| `--verbose` | CI logging takes precedence when not a TTY |
| `--stream` | CI logging disabled (streaming outputs directly) |
| `--dry-run` | No execution, no CI logging |

## Commands Affected

All three execution commands use `ProgressDisplay` and will get CI logging automatically:

- `aster <target>` (e.g., `aster build`, `aster test`)
- `aster run <targets...>`
- `aster affected <target>`

## No New CLI Flags

Auto-detection means zero configuration for users. The existing `--verbose` flag remains available for interactive terminals wanting detailed output.
