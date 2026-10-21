# Aster GitHub Action

Installs a pinned Aster release and runs one command with the GitHub Actions
cache token scoped to that process.

```yaml
- uses: ArchAstro/aster/action@v0.21.0
  with:
    run: run //app:build
```

`version` empty installs the latest stable release. `cache-mode` is `auto`
by default: `rw` on push, schedule, and workflow_dispatch to the default
branch, otherwise `read`. The runtime token is not exported to later steps.

Build `dist/` with `npm run build` after changing `src/`. Tests are
`npm test` and do not use the network.
