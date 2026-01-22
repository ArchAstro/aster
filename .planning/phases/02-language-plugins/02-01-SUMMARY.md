---
phase: 02
plan: 01
subsystem: plugins
tags: [elixir, python, regex, toml, language-plugins]

dependency-graph:
  requires: [01-01, 01-02, 01-03]
  provides: [elixir-plugin, python-plugin, polyglot-support]
  affects: [02-02, 03-01]

tech-stack:
  added:
    - regex: "1.10"
  patterns:
    - LazyLock for compiled regex (std::sync::LazyLock)
    - Serde untagged enum for variant parsing
    - Whitespace normalization for multiline regex

key-files:
  created:
    - src/plugins/elixir.rs
    - src/plugins/python.rs
  modified:
    - src/plugins/mod.rs
    - src/lib.rs
    - Cargo.toml
    - src/plugins/registry.rs

decisions:
  - id: elixir-regex
    choice: Regex extraction for mix.exs parsing
    rationale: Portability - no dependency on mix being installed
  - id: python-priority
    choice: PEP 621 project.name takes priority over tool.poetry.name
    rationale: PEP 621 is the Python standard; Poetry 2.0 supports both
  - id: umbrella-implicit-path
    choice: in_umbrella:true resolves to ../name
    rationale: Standard Elixir umbrella convention

metrics:
  duration: 3m 12s
  tests-added: 21
  tests-total: 80
  completed: 2026-01-22
---

# Phase 02 Plan 01: Language Plugins Summary

Implemented Elixir and Python language plugins enabling polyglot dependency graph construction.

## One-liner

Elixir mix.exs and Python pyproject.toml parsing via regex/TOML with Send+Sync plugins.

## What Was Built

### Elixir Plugin (src/plugins/elixir.rs)

ElixirPlugin implementing LanguagePlugin trait for mix.exs parsing:

- **Project metadata extraction:** Regex captures `app: :name` atom
- **Path dependencies:** `{:dep, path: "../path"}` format
- **Umbrella dependencies:** `{:sibling, in_umbrella: true}` resolves to `../sibling`
- **Multiline handling:** Whitespace normalization enables deps spanning multiple lines
- **8 tests** covering all parsing scenarios

Key implementation pattern:
```rust
static PATH_DEP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{:(\w+),\s*path:\s*"([^"]+)"(?:\s*,\s*in_umbrella:\s*(?:true|false))?\s*\}"#)
        .expect("Invalid regex")
});
```

### Python Plugin (src/plugins/python.rs)

PythonPlugin implementing LanguagePlugin trait for pyproject.toml parsing:

- **PEP 621 support:** `[project].name` and `dependencies` array
- **Poetry support:** `[tool.poetry].name` and `dependencies`/`dev-dependencies` tables
- **Priority:** PEP 621 takes precedence over Poetry when both present
- **Path formats:**
  - Poetry: `{path = "../lib"}` or `{path = "../lib", develop = true}`
  - PEP 621: `pkg @ file:../path`
- **10 tests** covering all parsing scenarios

Key implementation pattern:
```rust
#[derive(Deserialize)]
#[serde(untagged)]
enum PoetryDep {
    Version(String),
    Table(PoetryDepTable),
}
```

### Module Integration

- Added `pub mod elixir` and `pub mod python` to mod.rs
- Exported `ElixirPlugin` and `PythonPlugin` from plugins module
- Updated lib.rs re-exports for public API
- Added integration test verifying all three plugins work with PluginRegistry

## Commits

| Hash | Description |
|------|-------------|
| 13c8975 | feat(02-01): implement Elixir plugin with regex-based mix.exs parsing |
| 63048b8 | feat(02-01): implement Python plugin with TOML-based pyproject.toml parsing |
| c987d7c | feat(02-01): register plugins and update module exports |

## Test Results

```
running 70 tests (unit)
running 10 tests (integration)
test result: ok. 80 passed; 0 failed
```

New tests added: 21 (8 Elixir + 10 Python + 3 registry)

## Deviations from Plan

None - plan executed exactly as written.

## Dependencies Added

| Crate | Version | Purpose |
|-------|---------|---------|
| regex | 1.10 | Elixir mix.exs pattern matching |

Note: `toml` and `serde` were already present in Cargo.toml from Phase 1.

## Success Criteria Verification

- [x] ElixirPlugin parses mix.exs :app names and path: dependencies
- [x] PythonPlugin parses pyproject.toml project names and Poetry/PEP621 path dependencies
- [x] Both plugins integrate with PluginRegistry without breaking existing Node.js functionality
- [x] All tests pass including new plugin tests (80 total)
- [x] Plugins implement Send + Sync (required by LanguagePlugin trait)

## Next Phase Readiness

**02-02 (Target Resolution):**
- Plugins now provide language identification via `name()` method
- Target defaults can be mapped per plugin: nodejs/elixir/python
- Ready for TargetResolver implementation

**Blockers:** None

**Recommendations for 02-02:**
- Consider adding `default_targets()` method to LanguagePlugin trait
- Or create separate TargetDefaults struct as shown in RESEARCH.md
