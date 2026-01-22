---
status: complete
phase: 02-language-plugins
source: [02-01-SUMMARY.md, 02-02-SUMMARY.md]
started: 2026-01-22T21:45:00Z
updated: 2026-01-22T21:45:00Z
---

## Current Test

[testing complete]

## Tests

### 1. Elixir Path Dependency Parsing
expected: Elixir projects discovered from mix.exs. Path dependencies like `{:dep, path: "../lib"}` appear as graph edges.
result: pass

### 2. Elixir Umbrella Dependency
expected: Umbrella deps like `{:sibling, in_umbrella: true}` resolve to `../sibling` path.
result: pass

### 3. Python Poetry Path Dependency
expected: Python projects discovered from pyproject.toml. Poetry path deps like `{path = "../lib"}` appear as graph edges.
result: pass

### 4. Python PEP 621 File Dependency
expected: PEP 621 format deps like `pkg @ file:../path` in dependencies array are parsed into graph edges.
result: pass

### 5. Target Defaults - Node.js
expected: Node.js projects get default targets: test=`npm test`, build=`npm run build`, lint=`npm run lint`.
result: pass

### 6. Target Defaults - Elixir
expected: Elixir projects get default targets: test=`mix test`, build=`mix compile`, lint=`mix credo`.
result: pass

### 7. Target Defaults - Python
expected: Python projects get default targets: test=`pytest`, build=`python -m build`, lint=`ruff check .`.
result: pass

### 8. Target Override in aster.toml
expected: Custom target in aster.toml (e.g., `test = "yarn test"`) overrides the default while keeping other defaults.
result: pass

### 9. Polyglot Workspace Discovery
expected: Workspace with Node.js, Elixir, and Python projects discovers all three with correct plugin names.
result: pass

## Summary

total: 9
passed: 9
issues: 0
pending: 0
skipped: 0

## Gaps

[none yet]
