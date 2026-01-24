# Format Target Design

Add a `format` target to all language plugins for code formatting.

## Overview

Each plugin will detect available formatters and add a `format` target when appropriate tooling is detected.

## Detection Logic by Plugin

### Node.js
1. If `scripts.format` exists in package.json → `npm run format`
2. Else if Prettier config file exists → `npx prettier --write .`

Prettier config files:
- `.prettierrc`
- `.prettierrc.json`
- `.prettierrc.js`
- `.prettierrc.cjs`
- `.prettierrc.mjs`
- `.prettierrc.yml`
- `.prettierrc.yaml`
- `.prettierrc.toml`
- `prettier.config.js`
- `prettier.config.cjs`
- `prettier.config.mjs`

### Python
1. If `[tool.ruff]` exists in pyproject.toml → `ruff format .`
2. Else if `[tool.black]` exists in pyproject.toml → `black .`

### Go
- Always available (built-in) → `go fmt ./...`

### Elixir
- If `.formatter.exs` exists in project directory → `mix format`

## Target Configuration

All format targets will have:
- `depends_on: ["//self:deps", "//self:build"]`
- `capabilities: {}` (no FilesList - formatting runs on whole project)
- `files_glob: None`

## Files to Modify

- `src/plugins/nodejs.rs`
- `src/plugins/python.rs`
- `src/plugins/go.rs`
- `src/plugins/elixir.rs`
