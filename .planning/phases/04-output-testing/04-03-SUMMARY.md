---
phase: 04
plan: 03
subsystem: cli-testing
tags: [logs, integration-tests, assert_cmd, monorepo, json]

dependency-graph:
  requires: [04-02]
  provides: [logs-command, integration-tests, monorepo-tests]
  affects: []

tech-stack:
  added: [assert_cmd, predicates, dirs]
  patterns: [cli-integration-testing, ignored-tests-for-local-resources]

key-files:
  created:
    - tests/cli_tests/json_output.rs
    - tests/cli_tests/monorepo.rs
    - tests/cli_tests/mod.rs
  modified:
    - src/cli/commands.rs
    - src/main.rs
    - tests/integration.rs
    - Cargo.toml

decisions:
  - id: monorepo-tests-ignored
    choice: "Mark monorepo tests with #[ignore]"
    rationale: "Requires local ~/archastro/firstlanding-wt9, run with cargo test --ignored"
  - id: assert-cmd-for-cli-tests
    choice: "Use assert_cmd for CLI integration tests"
    rationale: "Standard Rust testing pattern for binary testing"
  - id: logs-missing-target-silent
    choice: "Exit silently with no output for missing target"
    rationale: "Per CONTEXT.md - not an error, just empty"

metrics:
  duration: ~10m
  completed: 2026-01-23
---

# Phase 04 Plan 03: Logs Command & Integration Tests Summary

**One-liner:** `aster logs` command with target-specific output retrieval, JSON output validation tests, and real monorepo integration tests against firstlanding-wt9.

## What Was Built

Completed the CLI with log retrieval and comprehensive integration testing:

1. **Logs Command** (`src/cli/commands.rs`, `src/main.rs`):
   - `aster logs` shows summary of last run's targets with PASS/FAIL/SKIP status
   - `aster logs //project:target` shows full output for specific target
   - `aster logs --json` outputs RunLog as JSON
   - Missing target/run exits silently (not an error)

2. **JSON Output Tests** (`tests/cli_tests/json_output.rs`):
   - Validates `aster list --json` produces valid JSON array
   - Validates `aster graph --json` has nodes/edges keys
   - Validates `aster why --json` has from/to/path keys
   - Validates `aster logs --json` produces valid JSON
   - Tests JSON flag position (before subcommand)

3. **Monorepo Integration Tests** (`tests/cli_tests/monorepo.rs`):
   - 8 tests against ~/archastro/firstlanding-wt9
   - Tests discovery, graph, list, affected, logs commands
   - All marked `#[ignore]` for opt-in execution
   - Read-only operations only (no state modification)
   - Validates polyglot detection (multiple plugin types)

4. **Test Infrastructure Fixes**:
   - Updated existing tests to match target-level graph output format
   - Cycle detection tests now use target addresses
   - Mixed language tests use explicit target dependencies

## Key Implementation Details

### Logs Command Flow
```
aster logs              -> LogStore::load_latest() -> print summary
aster logs //addr       -> LogStore::get_target_log(addr) -> print full output
aster logs --json       -> output as JSON to stdout
```

### Test Organization
```
tests/
  integration.rs        # Main test file, includes cli_tests module
  cli_tests/
    mod.rs              # Common utilities (setup_workspace, write_*)
    json_output.rs      # JSON output validation tests
    monorepo.rs         # Real monorepo tests (#[ignore])
```

### Monorepo Tests
Designed to validate against a real polyglot monorepo:
- `test_real_monorepo_discovery` - Projects found via aster list
- `test_real_monorepo_list_json` - JSON output structure
- `test_real_monorepo_graph_completes` - No cycles detected
- `test_real_monorepo_graph_json` - Graph JSON structure
- `test_real_monorepo_affected_read_only` - Affected command works
- `test_real_monorepo_logs_command` - Logs shows "No previous run"
- `test_real_monorepo_logs_json` - Logs JSON valid
- `test_real_monorepo_polyglot_detection` - Multiple plugin types

## Commits

| Hash | Type | Description |
|------|------|-------------|
| 6582950 | feat | Implement aster logs command |
| 689876e | test | Add JSON output and monorepo integration tests |
| c7dc784 | fix | Update integration tests for target-level graph output |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Updated integration tests for target-level graph format**
- **Found during:** Task 3 (monorepo test verification)
- **Issue:** Existing tests expected `-> //project` format but graph outputs target-level `-> [//project:target]`
- **Fix:** Updated tests to include scripts in package.json and check for target-level format
- **Files modified:** tests/integration.rs
- **Verification:** `cargo test` passes all 29 tests
- **Committed in:** c7dc784

---

**Total deviations:** 1 auto-fixed (bug - test assertions didn't match implementation)
**Impact on plan:** Essential fix for test accuracy. No scope creep.

## Success Criteria Verification

- [x] `aster logs` command shows last run summary
- [x] `aster logs <target>` shows full output for target
- [x] `aster logs --json` outputs JSON
- [x] Missing target/run handled gracefully (empty output)
- [x] JSON output tests pass for list, graph, why, logs
- [x] Real monorepo tests exist and pass when run with --ignored
- [x] All tests pass with `cargo test` (29 passed, 8 ignored)
- [x] TEST-02 requirement satisfied (integration tests against firstlanding-wt9)

## Running the Tests

```bash
# Run all regular tests
cargo test

# Run monorepo tests (requires ~/archastro/firstlanding-wt9)
cargo test monorepo -- --ignored

# Run JSON output tests specifically
cargo test json_output
```
