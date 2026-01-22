---
phase: 01
plan: 02
subsystem: discovery
tags: [nodejs, plugin, gitignore, toml]
dependency-graph:
  requires: [01-01]
  provides: [NodeJsPlugin, AsterToml, discover_projects, DiscoveredProject]
  affects: [01-03, 02-01]
tech-stack:
  added: []
  patterns: [language-plugin, builder-pattern, visitor-pattern]
key-files:
  created:
    - src/plugins/nodejs.rs
    - src/config/project.rs
    - src/discovery/mod.rs
    - src/discovery/scanner.rs
  modified:
    - src/plugins/mod.rs
    - src/config/mod.rs
    - src/lib.rs
decisions:
  - depends_on addresses stored as LocalDependency with address string as path
  - name collisions resolved by appending plugin name suffix (e.g., core-nodejs)
metrics:
  duration: 3m 1s
  completed: 2026-01-22
---

# Phase 1 Plan 2: Node.js Plugin & Project Discovery Summary

Node.js plugin parses package.json for name/version and file: dependencies; discovery uses WalkBuilder with gitignore support and merges aster.toml overrides.

## What Was Built

### Node.js Plugin (`src/plugins/nodejs.rs`)
- Implements `LanguagePlugin` trait for Node.js/npm projects
- Parses `package.json` to extract project name (required) and version (optional)
- Extracts `file:` protocol dependencies from both `dependencies` and `devDependencies`
- Resolves file paths relative to the project directory

### aster.toml Configuration (`src/config/project.rs`)
- `AsterToml` struct with three optional fields:
  - `name`: Override project name from native config
  - `depends_on`: Cross-language dependencies as address strings
  - `targets`: Custom command targets as key-value map
- `parse_aster_toml()` validates all `depends_on` entries are valid addresses
- `find_aster_toml()` helper checks for config file existence

### Project Discovery (`src/discovery/scanner.rs`)
- `DiscoveredProject` struct captures all discovery output:
  - Absolute root path and config file path
  - Merged metadata, dependencies, and targets
  - Plugin name and relative path for addressing
- `discover_projects()` function:
  - Uses `ignore::WalkBuilder` for directory traversal
  - Respects `.gitignore`, `.git/info/exclude`, and global gitignore
  - Finds marker files (e.g., `package.json`) and invokes matching plugin
  - Merges `aster.toml` overrides when present
  - Resolves name collisions with `{name}-{plugin}` suffix

## Commits

| Hash | Message |
|------|---------|
| 56bbf33 | feat(01-02): implement Node.js plugin for package.json parsing |
| dd92e19 | feat(01-02): implement aster.toml configuration parsing |
| df308fa | feat(01-02): implement project discovery with WalkBuilder |

## Test Coverage

**37 total tests passing** (includes 01-01 tests)

New tests added:
- `test_parse_simple_package_json` - Basic name/version extraction
- `test_parse_file_dependencies` - Extract file: deps from dependencies
- `test_parse_file_dev_dependencies` - Extract file: deps from devDependencies
- `test_parse_mixed_dependencies` - Only file: deps extracted, registry deps ignored
- `test_parse_missing_name` - Error on missing name field
- `test_parse_empty_name` - Error on empty name field
- `test_parse_invalid_json` - Error on malformed JSON
- `test_parse_minimal_aster_toml` - Just name override
- `test_parse_depends_on` - List of dependency addresses
- `test_parse_targets` - Custom target definitions
- `test_parse_full_aster_toml` - All fields together
- `test_parse_invalid_address` - Error on malformed depends_on
- `test_find_aster_toml_exists` - Returns Some when present
- `test_find_aster_toml_missing` - Returns None when absent
- `test_parse_empty_aster_toml` - Empty file parses to defaults
- `test_discover_single_project` - One package.json discovered
- `test_discover_multiple_projects` - Nested projects found
- `test_discover_respects_gitignore` - node_modules skipped
- `test_discover_merges_aster_toml` - Name override and targets applied
- `test_discover_name_collision` - Both projects get suffix
- `test_discover_empty_workspace` - No projects returns empty vec
- `test_discover_with_file_dependencies` - file: deps parsed correctly

## Decisions Made

1. **depends_on storage**: Address strings from `aster.toml` are stored in `LocalDependency` with the address string as the path. Resolution to actual paths happens during graph building (Phase 1 Plan 3).

2. **Name collision resolution**: When two projects have the same name, both are renamed to `{name}-{plugin}` (e.g., `core-nodejs`). This ensures unique names without requiring user intervention.

## Deviations from Plan

None - plan executed exactly as written.

## Files Changed

```
src/plugins/nodejs.rs      (new)  - 170 lines - Node.js plugin
src/config/project.rs      (new)  - 161 lines - aster.toml parsing
src/discovery/mod.rs       (new)  -   8 lines - Module declaration
src/discovery/scanner.rs   (new)  - 297 lines - Discovery implementation
src/plugins/mod.rs         (mod)  - Added nodejs module export
src/config/mod.rs          (mod)  - Added project module export
src/lib.rs                 (mod)  - Added discovery module and exports
```

## Next Phase Readiness

Ready for Plan 3 (Graph Building):
- `DiscoveredProject` provides all data needed to build graph nodes
- Dependencies include both file: paths and address strings
- Relative paths enable address construction (e.g., `//services/api`)
