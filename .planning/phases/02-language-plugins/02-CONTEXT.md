# Phase 2 Context: Language Plugins

**Created:** 2026-01-22
**Phase Goal:** Users can build dependency graphs from Elixir and Python projects alongside Node.js

## Decisions

### Elixir Parsing

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Parsing approach | Regex extraction | Portability - no dependency on mix being installed |
| Supported patterns | All common ones + mix workspaces | Umbrella projects are common in Elixir monorepos |
| Unsupported patterns | Error and stop | Suggest manual override in aster.toml; fail fast is better than silent wrong behavior |
| Project name extraction | Mix project :app atom | Standard Elixir convention, e.g., `def project do [app: :my_app, ...]` |

**Elixir path dependency patterns to support:**
- `{:dep_name, path: "../relative/path"}`
- `{:dep_name, path: "../path", in_umbrella: true}`
- Mix umbrella `apps/*/mix.exs` structure

### Python Parsing

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Config formats | Poetry + PEP 621 in pyproject.toml | Both are common; PEP 621 is the standard |
| Path dependency syntax | Both Poetry path and editable installs | `{path = "../lib"}` and `dep @ file:../lib` syntaxes |
| Project name extraction | pyproject.toml `[project].name` or `[tool.poetry].name` | Standard locations per spec |
| Fallback chain | pyproject.toml → setup.py → requirements.txt | Modern first, legacy fallback |
| setup.py parsing | Regex extraction | Avoid executing arbitrary Python code |
| requirements.txt | Claude's discretion | May skip or do basic `-e ./path` detection |
| Workspace detection | Yes, detect Poetry/hatch workspaces | Similar to mix umbrellas |

**Python path dependency patterns to support:**
- Poetry: `dep = {path = "../relative/path"}`
- Poetry: `dep = {path = "../path", develop = true}`
- PEP 621 editable: `dep @ file:../relative/path`
- requirements.txt: `-e ../relative/path` (optional)

### Target Mapping

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Core targets | test, build, lint | Universal across languages; covers 90% of use cases |
| Default commands | Conventional per language | npm test/build/lint, mix test/compile/credo, pytest/build/ruff |
| Missing target behavior | Warn and skip | Don't fail the whole run; user may not need all targets |
| Override behavior | Merge at target level | Individual keys in aster.toml override; unspecified keep defaults |

**Default target commands:**

| Language | test | build | lint |
|----------|------|-------|------|
| Node.js | `npm test` | `npm run build` | `npm run lint` |
| Elixir | `mix test` | `mix compile` | `mix credo` |
| Python | `pytest` | `python -m build` | `ruff check .` |

**Merge example:**
```toml
# aster.toml - only override test, keep default build/lint
[targets]
test = "npm run test:ci"
```

### Cross-Language Dependencies

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Declaration method | aster.toml `depends_on` only | Native configs can't express cross-language deps |
| Validation | Error if missing | Fail fast if referenced project doesn't exist in graph |

**Example:**
```toml
# services/python-api/aster.toml
depends_on = ["//libs/elixir-core", "//libs/shared-types"]
```

## Open Questions

None - all gray areas resolved.

## Constraints

- No shelling out to language runtimes (mix, python) for parsing
- Must handle malformed configs gracefully with actionable error messages
- Plugins must implement existing LanguagePlugin trait (Send + Sync)

## References

- Phase 1 plugin architecture: `src/plugins/mod.rs`
- Node.js plugin reference: `src/plugins/nodejs.rs`
- Requirements: PLUG-02, PLUG-03, PLUG-04
