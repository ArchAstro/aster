---
phase: 02-language-plugins
verified: 2026-01-22T21:42:33Z
status: passed
score: 3/3 must-haves verified
---

# Phase 2: Language Plugins Verification Report

**Phase Goal:** Users can build dependency graphs from Elixir and Python projects alongside Node.js
**Verified:** 2026-01-22T21:42:33Z
**Status:** passed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Elixir `path:` dependencies in mix.exs are correctly parsed into graph edges | ✓ VERIFIED | ElixirPlugin.parse_dependencies() extracts `{:name, path: "../path"}` patterns using regex. Test `test_parse_path_dependency` passes. Integration test `test_discover_elixir_project` confirms end-to-end graph construction with Elixir path deps. |
| 2 | Python path dependencies in pyproject.toml (Poetry format) are correctly parsed into graph edges | ✓ VERIFIED | PythonPlugin.parse_dependencies() extracts both Poetry `{path = "../lib"}` format and PEP 621 `pkg @ file:../path` format. Test `test_parse_poetry_path_dependency` passes. Integration test `test_discover_python_project` confirms end-to-end graph with Poetry deps. |
| 3 | Standard targets (test, build, lint) map to native commands for each language (mix test, npm test, pytest) | ✓ VERIFIED | TargetResolver.resolve() returns language-specific defaults: nodejs→npm test/build/lint, elixir→mix test/compile/credo, python→pytest/python -m build/ruff check. Tests `test_nodejs_defaults`, `test_elixir_defaults`, `test_python_defaults` all pass. Integration test `test_discover_target_defaults_by_plugin` confirms targets populated during discovery. |

**Score:** 3/3 truths verified

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `src/plugins/elixir.rs` | ElixirPlugin implementing LanguagePlugin trait | ✓ VERIFIED | 369 lines, implements all trait methods, 8 comprehensive tests, uses LazyLock regex for path: and in_umbrella: parsing |
| `src/plugins/python.rs` | PythonPlugin implementing LanguagePlugin trait | ✓ VERIFIED | 425 lines, implements all trait methods, 10 comprehensive tests, supports both Poetry and PEP 621 formats with proper priority |
| `src/targets/resolver.rs` | TargetResolver with default commands per language | ✓ VERIFIED | 146 lines, provides defaults_for_plugin() for nodejs/elixir/python, merge-at-key-level override strategy, 7 tests |
| `src/plugins/mod.rs` | Module exports for ElixirPlugin and PythonPlugin | ✓ VERIFIED | Lines 43-50: `pub mod elixir;`, `pub mod python;`, `pub use elixir::ElixirPlugin;`, `pub use python::PythonPlugin;` |
| `src/main.rs` | All three plugins registered in CLI | ✓ VERIFIED | Lines 40-42: NodeJsPlugin, ElixirPlugin, PythonPlugin all registered with PluginRegistry |
| `Cargo.toml` | regex dependency added | ✓ VERIFIED | regex = "1.10" dependency present for Elixir mix.exs parsing |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|----|--------|---------|
| ElixirPlugin | PluginRegistry | main.rs registration | ✓ WIRED | Line 41: `registry.register(Box::new(ElixirPlugin));` |
| PythonPlugin | PluginRegistry | main.rs registration | ✓ WIRED | Line 42: `registry.register(Box::new(PythonPlugin));` |
| scanner.rs | TargetResolver | resolve() call | ✓ WIRED | Line 140: `let targets = TargetResolver::resolve(plugin.name(), &custom_targets);` fills DiscoveredProject.targets |
| ElixirPlugin.parse_dependencies() | PATH_DEP_REGEX | regex capture | ✓ WIRED | Lines 71-77: regex captures `{:name, path: "path"}` pattern and constructs LocalDependency |
| PythonPlugin.parse_dependencies() | Poetry/PEP621 TOML | serde deserialize | ✓ WIRED | Lines 113-159: deserializes pyproject.toml and extracts path from PoetryDepTable.path and PEP621 file: format |
| TargetResolver.resolve() | defaults_for_plugin() | internal call | ✓ WIRED | Line 25: `let mut targets = defaults_for_plugin(plugin_name);` merges defaults with custom |

### Requirements Coverage

Phase 2 requirements from ROADMAP.md:

| Requirement | Status | Blocking Issue |
|-------------|--------|----------------|
| PLUG-02: Elixir plugin | ✓ SATISFIED | All truths verified, ElixirPlugin fully implemented and tested |
| PLUG-03: Python plugin | ✓ SATISFIED | All truths verified, PythonPlugin fully implemented and tested |
| PLUG-04: Target resolution | ✓ SATISFIED | All truths verified, TargetResolver provides per-language defaults |

### Anti-Patterns Found

**No blocking anti-patterns detected.**

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| src/plugins/python.rs | 51, 62-66 | `#[allow(dead_code)]` on unused enum variants/fields | ℹ️ Info | Intentional - serde needs all fields defined but not all are used in logic |

### Test Coverage

**Unit tests:** 78 passed
- Elixir plugin: 8 tests (simple project, path deps, umbrella deps, multiline, error cases)
- Python plugin: 10 tests (PEP621, Poetry, path deps, priority, mixed formats, error cases)
- Target resolver: 7 tests (all 3 language defaults, overrides, custom additions, unknown plugin)
- Registry: 3 tests (all plugins work together)

**Integration tests:** 14 passed
- `test_discover_elixir_project`: Creates mix.exs with path dependency, verifies graph construction
- `test_discover_python_project`: Creates pyproject.toml with Poetry path dep, verifies graph
- `test_discover_polyglot_workspace`: Creates Node.js + Elixir + Python projects, verifies all discovered
- `test_graph_with_mixed_languages`: Cross-language dependencies via aster.toml

**Total:** 92 tests passing, 0 failures

### Implementation Quality Checks

**Elixir Plugin:**
- ✓ Implements LanguagePlugin trait completely
- ✓ Uses LazyLock for compiled regex (performance best practice)
- ✓ Handles multiline dependencies via whitespace normalization
- ✓ Supports both explicit `path:` and implicit `in_umbrella: true` patterns
- ✓ Comprehensive error messages when app: name not found
- ✓ 8 tests covering all parsing scenarios

**Python Plugin:**
- ✓ Implements LanguagePlugin trait completely
- ✓ Uses serde #[serde(untagged)] for variant parsing (idiomatic)
- ✓ Proper priority: PEP 621 project.name > tool.poetry.name
- ✓ Extracts from both dependencies and dev-dependencies sections
- ✓ Supports both Poetry and PEP 621 file: formats
- ✓ 10 tests covering all parsing scenarios including mixed formats

**Target Resolver:**
- ✓ Merge-at-key-level strategy (custom test overrides only test, keeps build/lint defaults)
- ✓ Returns empty HashMap for unknown plugins (graceful degradation)
- ✓ Supports custom target additions (deploy, etc.) alongside defaults
- ✓ Integration with scanner confirmed via test_discover_target_defaults_by_plugin

**Wiring Verification:**
- ✓ All three plugins registered in main.rs (lines 40-42)
- ✓ Scanner calls TargetResolver.resolve() during discovery (line 140)
- ✓ ElixirPlugin and PythonPlugin exported from plugins module (mod.rs lines 43-50)
- ✓ targets module exported from lib.rs with TargetResolver re-export

### Manual Verification - Not Required

All success criteria can be verified programmatically through:
1. Unit tests confirm parsing logic for each plugin
2. Integration tests confirm end-to-end graph construction with real files
3. Code inspection confirms all wiring and exports are correct

No human verification needed - all observable truths are testable via automated tests.

---

## Verification Details

### Truth 1: Elixir path: dependencies parsed

**Evidence chain:**
1. **Artifact exists:** `src/plugins/elixir.rs` (369 lines)
2. **Implementation substantive:**
   - PATH_DEP_REGEX: `\{:(\w+),\s*path:\s*"([^"]+)"(?:\s*,\s*in_umbrella:\s*(?:true|false))?\s*\}`
   - parse_dependencies() iterates captures and constructs LocalDependency structs
   - Handles multiline via normalize_whitespace()
3. **Wired to system:**
   - ElixirPlugin registered in main.rs line 41
   - PluginRegistry.find_by_marker() returns ElixirPlugin for mix.exs files
   - scanner.rs calls plugin.parse_dependencies() for each discovered project
4. **Test verification:**
   - test_parse_path_dependency: Creates mix.exs with `{:shared_lib, path: "../shared_lib"}`, asserts extracted correctly
   - test_parse_multiple_path_dependencies: Verifies multiple path deps extracted
   - test_parse_umbrella_dependency: Verifies `in_umbrella: true` resolves to `../sibling_app`
   - Integration test_discover_elixir_project: End-to-end graph shows `-> //libs/core` dependency

**Status:** ✓ VERIFIED - Elixir path: dependencies are correctly parsed into graph edges

### Truth 2: Python Poetry path dependencies parsed

**Evidence chain:**
1. **Artifact exists:** `src/plugins/python.rs` (425 lines)
2. **Implementation substantive:**
   - PoetryDepTable struct with `path: Option<String>`
   - parse_dependencies() checks both dependencies and dev-dependencies sections
   - Also supports PEP 621 `pkg @ file:../path` format via PEP621_FILE_REGEX
   - Priority logic: PEP 621 project.name takes precedence over tool.poetry.name
3. **Wired to system:**
   - PythonPlugin registered in main.rs line 42
   - PluginRegistry.find_by_marker() returns PythonPlugin for pyproject.toml files
   - scanner.rs calls plugin.parse_dependencies() for each discovered project
4. **Test verification:**
   - test_parse_poetry_path_dependency: Creates pyproject.toml with `my-lib = {path = "../my-lib"}`, asserts extracted
   - test_parse_poetry_path_with_develop: Verifies `{path = "../shared", develop = true}` works
   - test_parse_poetry_dev_dependencies: Verifies dev-dependencies section parsed
   - test_mixed_poetry_and_pep621_deps: Both formats extracted in same file
   - Integration test_discover_python_project: End-to-end graph shows `-> //libs/utils` dependency

**Status:** ✓ VERIFIED - Python Poetry path dependencies are correctly parsed into graph edges

### Truth 3: Standard targets map to native commands per language

**Evidence chain:**
1. **Artifact exists:** `src/targets/resolver.rs` (146 lines)
2. **Implementation substantive:**
   - defaults_for_plugin() returns HashMap with test/build/lint commands
   - nodejs: "npm test", "npm run build", "npm run lint"
   - elixir: "mix test", "mix compile", "mix credo"
   - python: "pytest", "python -m build", "ruff check ."
   - resolve() merges defaults with custom targets at key level
3. **Wired to system:**
   - scanner.rs line 140: `let targets = TargetResolver::resolve(plugin.name(), &custom_targets);`
   - Result assigned to DiscoveredProject.targets field
   - TargetResolver exported from lib.rs for public API
4. **Test verification:**
   - test_nodejs_defaults: Asserts 3 npm commands returned
   - test_elixir_defaults: Asserts 3 mix commands returned
   - test_python_defaults: Asserts pytest/build/ruff commands returned
   - test_custom_override: Custom "test" overrides default, build/lint kept
   - test_custom_addition: Custom "deploy" added to 3 defaults = 4 total
   - test_discover_target_defaults_by_plugin: Integration test verifies targets populated during discovery

**Status:** ✓ VERIFIED - Standard targets map to native commands for each language

---

_Verified: 2026-01-22T21:42:33Z_
_Verifier: Claude (gsd-verifier)_
