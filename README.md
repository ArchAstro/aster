# aster

Aster is a build orchestrator for polyglot monorepos. It discovers projects,
connects cross-language dependencies, and runs targets in dependency order.

```console
aster test --all
aster build //services/api
aster affected test --base=main
```

## Installation

### Homebrew

```console
brew install ArchAstro/tap/aster
```

### From source

Aster requires Rust 1.88 or newer.

```console
cargo install --git https://github.com/ArchAstro/aster.git --locked
```

Prebuilt release archives are available for Linux x86-64, macOS x86-64, and
macOS Apple Silicon. Windows is not currently built or tested by the project.

### Native Linux packages

Tagged releases also include native x86-64 packages for Debian/Ubuntu, RPM-based
distributions, and Arch Linux. Download the package for your distribution from
the GitHub release, then install it with the system package manager:

```console
sudo apt install ./aster_VERSION_amd64.deb
sudo dnf install ./aster-VERSION-1.x86_64.rpm
sudo pacman -U ./aster-VERSION-1-x86_64.pkg.tar.zst
```

These packages currently target x86-64 Linux systems with glibc 2.35 or newer.

## Quick start

Run the same target across a workspace:

```console
aster test --all
aster build --all
aster lint --all
```

Select projects by address, directory, or the current directory:

```console
aster test //services/api
aster build //libs/core //libs/utils
aster test .
aster test 'services/*'
```

Dependencies run by default. Use `--no-deps` to omit them or `--dependents` to
include reverse dependencies.

Run different targets in one dependency-aware invocation:

```console
aster run //services/api:test //libs/core:build //tools/cli:lint
```

Explore what Aster discovered:

```console
aster list
aster graph
aster graph //services/api:build
aster why //services/api:test //libs/core:build
```

Use `aster --help` and `aster <command> --help` for the complete CLI reference.

## Supported languages

| Language | Marker | Common detected targets |
| --- | --- | --- |
| Rust | `Cargo.toml` | deps, build, test, lint, format, clean |
| Node.js | `package.json` | deps, build, test, lint, format, clean |
| Go | `go.mod` | deps, build, test, lint, clean |
| Python | `pyproject.toml` | deps, build, test, lint, format, clean |
| Elixir | `mix.exs` | deps, build, test, lint, format, clean |

Detected targets depend on the tools and scripts present in each project.
Node.js projects use the `packageManager` field or lockfile to choose npm or
pnpm. Workspace members inherit their workspace root's package manager.

## Project configuration

Place `aster.toml` beside a project's language marker:

```toml
name = "api"
depends_on = ["//libs/core:build"]

[targets]
lint = "npm run lint"
check = { alias = "test" }

[targets.test]
command = "npm test -- {files}"
depends_on = ["//self:build"]
capabilities = ["files_list"]
files_glob = "**/*.test.ts"
exclusive_resources = ["database"]

[targets.test.cache]
enabled = true
include = ["config/**/*.json"]
exclude = ["**/*.generated.ts"]
env = ["CI"]
outputs = ["coverage/summary.json"]
```

Run `aster project init` to generate a starter file.

Target commands are parsed like a shell command line for quoting, escaping, and
leading `NAME=value` environment assignments, but they are executed directly.
Shell operators such as pipes, redirects, `&&`, substitutions, and glob
expansion are not interpreted. If shell behavior is intentional, invoke it
explicitly, for example:

```toml
[targets.generate]
command = "sh -c 'generator | formatter > src/generated.rs'"
cache = { enabled = false }
```

Only the `files_list` capability is supported. When present, `{files}` is
required to be a standalone command argument and is safely expanded into
individual path arguments. It cannot be embedded in a quoted shell script or
combined argument, used as the executable, or passed directly through a command
interpreter. Use a fixed wrapper executable for more complex handling.
Affected paths are filtered by `files_glob`.
Unknown fields, capabilities, dependencies, and invalid globs are configuration
errors.

### Cache behavior

Aster's local cache memoizes successful target executions; it does not restore
artifacts. The default cacheable target names are `deps`, `build`, `test`,
`lint`, `format`, `typecheck`, and `check`. Other targets must opt in with
`cache.enabled = true`. Set `enabled = false` to opt out.

`include`, `exclude`, and `env` extend a target's detected cache inputs.
Configured `outputs` must still exist for a cached success to be reused. Clean
targets invalidate the project's cache after succeeding.

## Affected projects

```console
aster affected test --base=main
aster affected test --base=main --dependents
aster affected test --dry-run
```

Aster compares `HEAD` with the merge base of `HEAD` and `--base`, then includes
uncommitted changes. CI must fetch enough history for the merge base to exist.

Workspace-relative files can be excluded from affected analysis in the root
`aster.toml`:

```toml
[affected]
ignore = [".agents/**", "docs/generated/**"]
```

The root-level `ignore` list separately controls project discovery.

## Watch mode

```console
aster watch //services/api:build
aster watch //services/api:dev --debounce 500ms
```

Watch mode observes the requested targets and their transitive dependencies.
Non-stream prerequisites run before a `stream = true` target starts. Relevant
source changes received during a build are preserved for the next cycle.

Configure filesystem behavior in the root `aster.toml`:

```toml
[watch]
ignore = ["coverage/**"]
suppress_paths = ["services/web/priv/static/assets/**"]
debounce_ms = 300
```

Built-in ignores cover common VCS, dependency, and build-output directories.
During the cooldown window, only configured `suppress_paths` are dropped; use
them for generated paths that would otherwise create feedback loops.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and pull-request
checks. Please report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md). Community expectations and project decision-making
are documented in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) and
[GOVERNANCE.md](GOVERNANCE.md).

## License

[MIT](LICENSE)
