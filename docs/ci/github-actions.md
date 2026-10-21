# GitHub Actions

`aster ci cache-plan` prints the native tool caches (Cargo registry, npm
cache, `node_modules`, and the rest) that a workflow can hand to
[`actions/cache`](https://github.com/actions/cache). These are warm-start
directories for the language tools. They are not Aster's content-addressed
artifact cache, which memoizes target success and does not restore build
outputs.

A workflow cannot loop over a dynamic list of steps, and `actions/cache`
must run in the same job as the step that produced its inputs. The plan
therefore also emits one **combined** entry covering every path. Restore
and save that one entry. Do not matrix the job, and do not drive
`actions/cache` from `actions/github-script`.

## Workflow

Install Aster before the `plan` step (a release binary, `cargo install`, or
whatever this repository already uses). The `plan` step id is required:
restore and save read `steps.plan.outputs`.

```yaml
name: CI

on:
  push:
  pull_request:

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Aster
        run: cargo install aster --locked

      - name: Plan native caches
        id: plan
        run: aster ci cache-plan --github-output

      - name: Restore native caches
        uses: actions/cache/restore@v4
        with:
          path: ${{ steps.plan.outputs.paths }}
          key: ${{ steps.plan.outputs.key }}
          restore-keys: ${{ steps.plan.outputs.restore-keys }}

      - name: Build and test
        run: aster test --all

      - name: Save native caches
        if: github.event_name == 'push' && github.ref == format('refs/heads/{0}', github.event.repository.default_branch)
        uses: actions/cache/save@v4
        with:
          path: ${{ steps.plan.outputs.paths }}
          key: ${{ steps.plan.outputs.key }}
```

`aster ci cache-plan --github-output` appends the combined entry to
`$GITHUB_OUTPUT`:

| Output | Contents |
| --- | --- |
| `paths` | Every cache directory, one per line. `actions/cache` splits that list on newlines. |
| `key` | `aster-l0-all-<os>-<arch>-<sha256>`. The digest covers the sorted per-cache keys, so a lockfile change misses the exact key. |
| `restore-keys` | `aster-l0-all-<os>-<arch>-`. A prefix hit restores the newest cache for this OS and CPU when the exact key misses. |

`~` in a path is left for `actions/cache` to expand. A path written as
`$(command)` is the command's trimmed output, resolved while planning.

Save runs only on a push to the repository's default branch. Pull requests
still restore. `github.event.repository.default_branch` is the branch name
(`main`, `master`, or whatever the repository uses), so the comparison is
against `refs/heads/<that name>`, not a hard-coded branch.

Pass the same project selectors as `aster run` when a job should cache only
part of the workspace: `aster ci cache-plan --github-output //apps/web`.
With no addresses, every discovered project is included.

## Cache scope

GitHub's cache has three limits that change what this workflow can share:

- **10 GB per repository.** GitHub evicts the least recently used entries
  once a repository's caches exceed 10 GB. A large `target/` or
  `node_modules` tree can push other jobs' caches out.
- **7-day eviction.** An entry that is not accessed for 7 days is removed.
  A weekly default-branch push keeps the combined entry alive; a quiet
  repository starts cold.
- **Pull request saves are visible only to that pull request.** A cache
  saved from a pull request cannot be restored by another pull request or
  by a branch push. That is why this workflow saves only on a push to the
  default branch: that entry is visible to later pull requests and branch
  builds. A pull request that saved its own cache would warm only itself.

`restore-keys` can still match an older default-branch entry when the exact
key misses. The restored tree may be stale; the language tool then fills in
what the lockfile change requires.

## JSON for custom tooling

`--json` prints the same plan for a workflow that cannot use
`--github-output`. `entries` is one object per native cache, sorted by
`id`. `combined` is the single entry the workflow above restores and saves.
JSON uses `restore_keys`; `$GITHUB_OUTPUT` uses `restore-keys`, which is
the input name `actions/cache` reads.

```json
{
  "entries": [
    {
      "id": "cargo-registry",
      "paths": [
        "~/.cargo/registry/index",
        "~/.cargo/registry/cache",
        "~/.cargo/git/db"
      ],
      "key": "aster-l0-cargo-registry-linux-x86_64-<sha256>",
      "restore_keys": ["aster-l0-cargo-registry-linux-x86_64-"],
      "projects": ["//crates/a", "//crates/b"]
    }
  ],
  "combined": {
    "paths": ["~/.cargo/registry/index", "target", "apps/web/node_modules"],
    "key": "aster-l0-all-linux-x86_64-<sha256>",
    "restore_keys": ["aster-l0-all-linux-x86_64-"]
  }
}
```

`id` is the plugin's stable cache id. `projects` lists the Aster addresses
whose native cache collapsed into that entry. Shared caches with the same
id, paths, and key material are emitted once; two modules that share a
directory but hash different lockfiles stay separate, and both paths still
appear once in `combined.paths`.

`entries` is for inspection and for tooling that builds its own cache keys.
Do not loop it into one `actions/cache` step per entry: Actions expands
`uses:` steps before the job runs, so a dynamic list cannot become a
dynamic number of steps.

## Install Aster and share its artifact cache

`action/` is a JavaScript action that installs a pinned Aster release and
runs one command. Use it as `ArchAstro/aster/action@vX`, where `vX` is an
Aster release tag.

```yaml
- uses: ArchAstro/aster/action@v0.21.0
  with:
    run: run //app:build
```

`cache-mode` defaults to `auto`. That writes on `push`, `schedule`, and
`workflow_dispatch` to the repository's default branch, and reads otherwise.
`rw`, `read`, and `off` override it. The action passes
`ACTIONS_RESULTS_URL`, `ACTIONS_RUNTIME_TOKEN`, `ASTER_REMOTE_CACHE=gha`,
and `ASTER_CACHE_MODE` only to the spawned `aster` process. Later steps do
not receive the runtime token.
