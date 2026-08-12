# Using Aster

This is a practical Markdown reference for Aster, a dependency-aware task runner
and local-service supervisor for polyglot repositories. It describes commands,
selection syntax, configuration, and output. It does not define policy for a
human or an LLM.

## Mental model

Aster finds the workspace root from the repository's root `aster.toml` or Git
root. It discovers projects from language and build markers such as
`package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`, `mix.exs`, Gemfiles,
gemspecs, Gradle builds, and Maven POMs. A project address is workspace-relative:

- `//services/api` is a project.
- `//services/api:test` is one target in that project.
- `//self:test` refers to a target in the project containing the current
  `aster.toml`.

Targets such as `deps`, `build`, `test`, `lint`, `format`, `typecheck`, `dev`,
and `clean` come from detected tooling or explicit `aster.toml` configuration.
Aster builds a target graph, runs prerequisites first, parallelizes independent
work, and caches eligible successful results.

## Inspect a workspace before running work

```console
aster list
aster list --json
aster graph
aster graph //services/api:test
aster why //services/api:test //libs/core:build
```

`list` shows discovered projects, languages, build systems, and targets.
`graph` shows dependencies. `why` explains a dependency path between two target
addresses. `--json` provides structured output where supported.

## Select projects

Selection syntax is shared by ordinary target commands:

```text
//services/api       one exact project
//services/...       every project below a path
//...                every project in the workspace
./...                every project below the current directory
.                    the project containing the current directory
-//vendor/...        exclude a project subtree
```

Examples:

```console
aster test //services/api
aster test //services/...
aster test ./...
aster test --all
aster test //... -//vendor/...
aster lint --all --lang rust,nodejs
```

Dependencies are included by default. `--no-deps` limits execution to the
selected projects. `--dependents` also includes projects that depend on the
selection.

## Build, lint, format, type-check, and test

Target names are invoked directly:

```console
aster build --all
aster lint --all
aster format --all
aster typecheck --all
aster test --all
aster test //services/api --no-deps
aster build //libs/core --dependents
aster test --all --warnings-as-errors
```

Only projects exposing the requested target participate. A target named like a
built-in command can be invoked through the escape hatch:

```console
aster target services //tools/example
aster target logs //tools/example
```

Different targets can share one dependency-aware run:

```console
aster run //services/api:test //libs/core:build //tools/cli:lint
aster run //services/api:test //libs/core:build --no-deps
```

Useful global output flags include `--verbose`, `--quiet`, `--json`,
`--full-logs`, and `--no-cache`. Global flags can appear before or after the
command.

## Run only work affected by Git changes

```console
aster affected test --base=main
aster affected test --base=origin/main --dependents
aster affected lint --base=HEAD --dry-run
aster affected test --base=main --only-affected-files
aster affected test --base=main --warnings-as-errors
```

Affected analysis compares against the merge base of `HEAD` and `--base`, then
adds uncommitted changes. `--head <ref>` compares committed refs explicitly.
`--dry-run` previews selection. `--only-affected-files` narrows targets that
declare the `files_list` capability. CI checkouts need enough Git history to
resolve the merge base.

Workspace paths can be excluded from affected analysis in root `aster.toml`:

```toml
[affected]
ignore = ["docs/generated/**", ".agents/**"]
```

## Watch targets while editing

```console
aster watch //services/api:build
aster watch //services/api --target test
aster watch //services/api:dev --debounce 500ms
aster watch //services/api:build --no-initial
```

Watch mode observes the selected targets and their transitive dependencies.
Relevant changes rerun ordinary targets. Targets configured with `stream = true`
stay running and restart after relevant changes.

Root configuration can extend ignores, suppress generated feedback paths, and
set the default debounce:

```toml
[watch]
ignore = ["coverage/**"]
suppress_paths = ["services/web/priv/static/assets/**"]
debounce_ms = 300
```

## Run long-lived development services

Services map stable names to `stream = true` targets in root `aster.toml`:

```toml
[dev.ports.api]
allocation = "dynamic"
range = [4000, 4099]
preferred = 4000

[dev.ports.web]
default = 3000
offset_from = "api"
offset_base = 4000

[dev.services.api]
target = "//services/api:dev"
port = "api"
port_env = { PORT = "api" }
order = 10

[dev.services.web]
target = "//services/web:dev"
port = "web"
port_env = { PORT = "web" }
env = { API_URL = "http://localhost:{ports.api}" }
order = 20

[dev.ports.intern-control]
default = 5001

[dev.service_groups]
main = ["api", "web"]
intern = { services = ["intern-postgres", "intern-api", "intern-web"], control_port = "intern-control" }
```

Run the default stack or one named group:

```console
aster services up
aster services up intern
aster services up --dry-run
aster services up --no-ui
aster services up --no-watch
```

With no group argument, the `main` group and services absent from every group
run. If no `main` group exists, all ungrouped services run. An explicit group
runs exactly that group. The default terminal UI supports service tabs, search,
scrolling, restart, wrapping, copy, and browser opening; `?` shows its controls.
`--no-ui` emits line-oriented logs for non-interactive environments.
Array groups use the global `[dev].control_port`. A detailed group may override
it with its own named `control_port`, so multiple groups can run concurrently.
Static ports retain their configured values. Dynamic named ports are selected
from their ranges as one collision-free bundle per supervisor and released at
exit. Use `port_env` for direct numeric environment values and `{ports.name}`
inside `env` or target commands for URLs and other composite values.

## Serve trusted local HTTPS

Use a `tls_proxy` service when browsers need a trusted `.dev` or other HTTPS
origin. The edge uses named Aster ports for upstreams and belongs in service
groups like an ordinary service.

```toml
[dev.ports.https]
default = 8443

[dev.services.local-edge]
port = "https"
tls_proxy = { certificate_hosts = ["app.example.test", "*.local.example.test"], open_host = "app.example.test", dns_domain = "example.test", routes = [{ host = "app.example.test", upstream_port = "web" }, { host_suffix = ".local.example.test", open_host = "demo.local.example.test", upstream_port = "api" }] }
```

```fish
brew install mkcert dnsmasq
aster services tls setup local-edge
aster services up
```

Setup explicitly installs the mkcert local CA and writes the certificate under
`.aster/tls/local-edge/`. Normal service startup never installs packages,
changes trust stores, or edits DNS. Configure wildcard DNS separately so the
configured `dns_domain` resolves to `127.0.0.1`. If Chrome still reports
`ERR_NAME_NOT_RESOLVED`, fully quit and reopen Chrome and disable Secure DNS if
it is bypassing the system resolver.

For a selected TLS edge, `[open]` on an upstream service uses its HTTPS route.
Exact routes infer the hostname from `host`. To publish an open URL for a
suffix route, configure a concrete `open_host` matching the suffix and
certificate.

## Read service logs and clear occupied ports

Service output is persisted at
`.aster/logs/<worktree>/<service>/logs.txt`. Each file is capped at 10 MiB.

```console
aster services logs api
aster services logs api | grep ERROR
aster services logs api > api.log
```

Interactive `services logs` output honors `$PAGER`, then tries `less` and
`more`. Piped or redirected output is raw log text on stdout.

Configured development ports can be inspected or cleared:

```console
aster services kill-ports --dry-run
aster services kill-ports
aster services kill-ports api web 4011
```

Named ports come from `[dev.ports]` and this worktree's active or crash-left
dynamic allocation manifests. Explicit numeric ports can be inspected or
cleared outside an Aster workspace.

## Read target logs and manage the cache

Ordinary completed targets have a separate execution-log store:

```console
aster logs
aster logs //services/api:test
```

`aster logs` refers to target execution, while `aster services logs <name>`
refers to a supervised long-lived service.

```console
aster cache status
aster cache status //services/api:test
aster cache clear //services/api:test
aster cache clear
aster test //services/api --no-cache
```

The cache memoizes successful execution. It does not restore deleted artifacts.
Declared outputs must still exist for a cached result to be reusable.

## Configure custom projects and targets

`aster init` creates a workspace file. `aster project init .` creates starter
project configuration. A project-level `aster.toml` can define dependencies,
targets, aliases, cache inputs, outputs, and capabilities:

```toml
name = "api"
depends_on = ["//libs/core"]

[targets.lint]
command = "npm run lint"

[targets.test]
command = "npm test -- {files}"
depends_on = ["//self:build", "//libs/core:build"]
capabilities = ["files_list"]
files_glob = "**/*.test.ts"

[targets.test.cache]
enabled = true
include = ["config/**/*.json"]
env = ["CI"]
outputs = ["coverage/summary.json"]
```

Target commands use shell-style quoting but execute the parsed program directly.
Pipes, redirects, substitutions, and `&&` require an explicit shell command such
as `sh -c 'generator | formatter > output'`. `{files}` is expanded safely only
for targets declaring `files_list` and must occupy a standalone argument.

## Common end-to-end flows

Understand and validate an unfamiliar workspace:

```console
aster list --json
aster graph //services/api:test
aster affected test --base=main --dependents --dry-run
aster affected test --base=main --dependents
```

Develop with a local stack and inspect a failure:

```console
aster services up
aster services logs api | grep -i error
aster test //services/api --full-logs
aster logs //services/api:test
```

Run a broad local quality pass:

```console
aster format --all
aster lint --all --warnings-as-errors
aster typecheck --all
aster test --all
aster build --all
```

## More command detail

```console
aster --help
aster <command> --help
aster services --help
aster services up --help
```

Help output is the exact reference for the installed Aster version. This guide
is emitted by that same binary with `aster --skills` and requires no workspace.
