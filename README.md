# aster

*From the Greek ἀστήρ (astḗr) — "star"*

In the constellation of a polyglot monorepo, each project is a star. Alone, they twinkle. Together, they form something greater. **aster** is the force that binds them — orchestrating builds across languages, respecting the ancient paths of dependency, ensuring that when one star shines, all the stars it depends upon have already risen.

Born from [ArchAstro](https://github.com/ArchAstro), where we architect among the stars.

---

## What is aster?

A build orchestration tool for polyglot monorepos. It automatically discovers projects (Node.js, Go, Python, Elixir), understands their dependencies, and runs targets (test, build, lint) in the correct order.

```
aster test --all              # Test everything, dependencies first
aster build //services/api    # Build one project and its dependencies
aster affected test           # Only test what changed
```

## Installation

### Homebrew (private tap)

Requires GitHub authentication for private repo access:

```sh
# Set up token (add to ~/.zshrc to persist)
export HOMEBREW_GITHUB_API_TOKEN="$(gh auth token)"

# Install
brew install ArchAstro/tap/aster
```

### From source

```sh
cargo install --git git@github.com:ArchAstro/aster.git
```

## Usage

### Running targets

```sh
# Run a target on all projects
aster test --all
aster build --all
aster lint --all

# Run on specific projects (with dependencies)
aster test //services/api
aster build //libs/core //libs/utils

# Run without dependencies
aster test //services/api --no-deps

# Run on project in current directory
aster test .

# Include dependents (reverse dependencies)
aster build //libs/core --dependents
```

### Affected projects

Run targets only on projects changed since a base branch:

```sh
# Test projects affected by changes since main
aster affected test --base=main

# Show what would run without executing
aster affected test --dry-run

# Include dependents of affected projects
aster affected test --dependents
```

### Heterogeneous runs

Run different targets on different projects:

```sh
aster run //services/api:test //libs/core:build //tools/cli:lint
```

### Exploring the workspace

```sh
# List all projects
aster list

# List projects in a directory
aster list services/

# Show dependency graph
aster graph

# Show dependencies for a specific target
aster graph //services/api:build

# Find why one target depends on another
aster why //services/api:test //libs/core:build
```

## Project Configuration

Create `aster.toml` in any project to customize targets:

```toml
# Add dependencies on other projects
depends_on = ["//libs/core:build"]

# Override or add targets
[targets]
lint = "npm run eslint"
typecheck = "tsc --noEmit"

# Rich target configuration
[targets.test]
command = "npm test"
depends_on = ["//self:build", "//libs/core:build"]

# File-aware targets for affected runs
[targets.test]
command = "npm test -- {files}"
files_glob = "**/*.test.ts"
capabilities = ["FilesList"]

# Alias another target (clones command, deps, capabilities, etc.)
[targets]
check = { alias = "test" }

# Alias with additional dependencies
[targets.ci]
alias = "test"
depends_on = ["//self:lint", "//self:typecheck"]
```

Generate a starter config:

```sh
aster project init           # In current directory
aster project init ./myproj  # In specific directory
```

## Supported Languages

| Language | Marker File | Auto-detected Targets |
|----------|-------------|----------------------|
| Node.js | `package.json` | deps, build, test, lint (from scripts) |
| Go | `go.mod` | deps, build, test, lint (if golangci config) |
| Python | `pyproject.toml` | deps, build, test, lint |
| Elixir | `mix.exs` | deps, build, test, lint |

## License

MIT
