# Aster GitHub Action

Installs a pinned Aster release and runs one command with the GitHub Actions
cache token scoped to that process.

```yaml
- uses: ArchAstro/aster/action@v0.30.1
  with:
    version: "0.30.1"
    run: run //app:build
```

`run` is arguments passed to the `aster` binary, not a shell line and not the
program name. `run: aster build //app` fails: the action already invokes
`aster`, and the message is `run takes aster arguments, not the program name`.
Quotes are interpreted and removed, so `run: test 'apps/*'` passes `apps/*`
as one argument. An unbalanced quote is an error. Globs and variables are not
expanded.

Pin `version` (a leading `v` is optional). An empty `version` looks up the
latest stable release on the GitHub API. That lookup is unauthenticated unless
the step sets `GITHUB_TOKEN`, and shared runners are limited to 60 requests an
hour per IP. A 403 tells you to pin `version:` or pass the token:

```yaml
env:
  GITHUB_TOKEN: ${{ github.token }}
```

`cache-mode` is `auto` by default: `rw` on push, schedule, and
workflow_dispatch to the default branch, otherwise `read`. The runtime token
is not exported to later steps.

Build `dist/` with `npm run build` after changing `src/`. Tests are
`npm test` and do not use the network.
