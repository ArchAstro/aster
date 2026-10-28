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

## Artifact cache (L1)

L0 above warms language-tool directories. L1 is Aster's content-addressed
artifact cache: a hit restores a target's declared `outputs` and skips the
command. On GitHub Actions that store is the repository's Actions cache.
[`ArchAstro/aster/action`](../../action/README.md) installs a pinned Aster
release and runs one command with the cache token scoped to that process.

Use a release tag, not a branch: `ArchAstro/aster/action@v0.21.0`. `version`
empty installs the latest stable release. `cache-mode` defaults to `auto`:

| Event | Ref | Mode |
| --- | --- | --- |
| `push`, `schedule`, `workflow_dispatch` | default branch | `rw` (read and write) |
| anything else, including `pull_request` | any | `read` |

`rw`, `read`, and `off` override `auto`. The action sets
`ACTIONS_RESULTS_URL`, `ACTIONS_RUNTIME_TOKEN`, `ASTER_REMOTE_CACHE=gha`,
and `ASTER_CACHE_MODE` only on the spawned `aster` process. Later steps do
not receive the runtime token.

### Trust model

A pull request must not publish artifacts that later default-branch builds
restore. Two independent limits enforce that:

- **The action is read-only on a pull request.** `auto` selects `read`
  unless the event is `push`, `schedule`, or `workflow_dispatch` and `ref`
  is `refs/heads/<default branch>`. A fork pull request, a same-repo pull
  request, and a push to a feature branch all restore and do not upload.
- **GitHub isolates a pull-request cache write to that pull request's merge
  ref.** Even if a workflow overrides `cache-mode` to `rw`, an entry saved
  from `pull_request` is visible only to later jobs of that same pull
  request. It is not restored by another pull request or by a branch push.
  Default-branch writes are the entries every later job can restore.

Do not set `cache-mode: rw` on untrusted pull requests. `auto` is the
setting that matches this model.

### Combined workflow

Restore the L0 native caches, then let the action run Aster against the L1
artifact cache. Save L0 only from the default branch, matching the action's
write rule. The nightly job below is what keeps both stores warm.

```yaml
name: CI

on:
  push:
  pull_request:
  schedule:
    # 06:17 UTC every day. A fixed minute avoids the top-of-hour stampede.
    - cron: "17 6 * * *"

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Plan native caches
        id: plan
        # The action installs Aster for the build. Planning needs a binary
        # first; pin the same release the action step uses.
        run: |
          cargo install aster --locked --version 0.21.0
          aster ci cache-plan --github-output

      - name: Restore native caches
        uses: actions/cache/restore@v4
        with:
          path: ${{ steps.plan.outputs.paths }}
          key: ${{ steps.plan.outputs.key }}
          restore-keys: ${{ steps.plan.outputs.restore-keys }}

      - name: Build and test
        uses: ArchAstro/aster/action@v0.21.0
        with:
          version: "0.21.0"
          run: test --all

      - name: Save native caches
        if: >-
          (github.event_name == 'push' ||
           github.event_name == 'schedule' ||
           github.event_name == 'workflow_dispatch') &&
          github.ref == format('refs/heads/{0}', github.event.repository.default_branch)
        uses: actions/cache/save@v4
        with:
          path: ${{ steps.plan.outputs.paths }}
          key: ${{ steps.plan.outputs.key }}
```

`cache-mode` is left at `auto`, so the action writes the artifact cache on
the same events that save the native caches, and reads on pull requests.

### Nightly warm job

GitHub deletes a cache entry that has not been accessed for 7 days. A
repository whose default branch is quiet for a week starts cold. The
`schedule` trigger above runs the same job on the default branch every
night: L0 save is allowed, and `auto` selects `rw`, so both stores are
touched and kept. The cron does not run on pull requests.

A schedule always checks out the default branch. It does not rebuild a
feature branch. If the default branch already has a fresh cache from a
push that day, the nightly run is a cheap hit plus a save of the same L0
key.

### Actions cache limits

The artifact cache and the L0 `actions/cache` entries share one repository
cache, and GitHub applies the same limits to both:

- **10 GB per repository.** Least-recently-used entries are evicted once
  the repository exceeds 10 GB. A large Cargo `target/` or `node_modules`
  tree can push artifact entries out, and the other way around.
- **200 cache entries created per minute.** Creating entries faster than
  that returns HTTP 429. Aster stops remote writes for the rest of that
  run after a 429 and keeps going on the local store. Reads are unaffected.
  A job that uploads one entry per small target can hit this before it hits
  10 GB; pack targets, or accept that the tail of the run will not upload.
- **7-day eviction.** An entry that is not accessed for 7 days is removed.
  The nightly job above is what keeps a quiet repository warm.

Since 2025-11-20, a repository on a paid GitHub plan (Pro, Team, or
Enterprise) can raise the 10 GB size limit. Usage above 10 GB is billed per
GB-month. The 10 GB included with every repository stays free. Enterprise,
organization, and repository admins set the size and retention policies;
raising the limit does not raise the 200 entries/minute create rate. See
[GitHub Actions cache size can now exceed 10 GB per repository](https://github.blog/changelog/2025-11-20-github-actions-cache-size-can-now-exceed-10-gb-per-repository/).

### When the Actions cache is too small

Move the artifact cache to the HTTP backend when the repository outgrows
the Actions cache: the 10 GB cap (or the paid cap) is full, eviction is
throwing away entries you still need, or the 200 entries/minute create rate
stops uploads on ordinary builds. L0 native caches can stay on
`actions/cache`. Only L1 moves.

Point Aster at a [bazel-remote](https://github.com/buchgr/bazel-remote)
compatible server. The action always sets `ASTER_REMOTE_CACHE=gha` on the
process it spawns, and it does not add that binary to `PATH`, so a later
step cannot reuse it or override the backend. Install the same release
yourself and pass the HTTP settings only to that process. Keep the write
rule the action uses: read-write on the default branch, read-only on pull
requests.

```yaml
- name: Install Aster
  run: cargo install aster --locked --version 0.21.0

- name: Build and test
  env:
    ASTER_REMOTE_CACHE: http
    ASTER_REMOTE_CACHE_URL: https://cache.example.com
    ASTER_REMOTE_CACHE_TOKEN: ${{ secrets.ASTER_REMOTE_CACHE_TOKEN }}
    ASTER_CACHE_MODE: ${{ case((github.event_name == 'push' || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch') && github.ref == format('refs/heads/{0}', github.event.repository.default_branch), 'rw', 'read') }}
  run: aster test --all
```

`ASTER_REMOTE_CACHE=http` selects the HTTP backend.
`ASTER_REMOTE_CACHE_URL` is the server base URL (`http://` or `https://`).
`ASTER_REMOTE_CACHE_TOKEN` is an optional bearer token and is not logged.
Manifests are `GET`/`PUT` under `<base>/ac/<key>`; blobs are under
`<base>/cas/<sha256>`. A write that the server refuses (401 or 403) switches
that process to read-only. `ASTER_CACHE_MODE=read` on a pull request is the
setting that matches the trust model above, even when the token can upload.

## Fanning lanes out as a job matrix

[`aster affected --matrix`](../../README.md#fanning-lanes-out-as-a-ci-matrix)
prints one JSON row per lane the change selects. A plan job turns the rows
into `strategy.matrix.include`; each test job installs the row's
`toolchains` and runs its lane. No other selection logic lives in the
workflow.

```toml
# aster.toml
[affected.lanes.backend]
include = ["//services/..."]
matrix = { runs_on = "ubuntu-latest" }

[affected.lanes.web]
include = ["//web/..."]
matrix = { runs_on = "ubuntu-latest-8-cores" }
```

```yaml
jobs:
  plan:
    runs-on: ubuntu-latest
    outputs:
      rows: ${{ steps.plan.outputs.rows }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Install Aster
        run: cargo install aster --locked
      - name: Plan lanes
        id: plan
        run: |
          if [ "${{ github.event_name }}" = schedule ]; then
            echo "rows=$(aster affected test --all --matrix)" >> "$GITHUB_OUTPUT"
          else
            echo "rows=$(aster affected test --base=origin/${{ github.base_ref || 'main' }} --related --matrix)" >> "$GITHUB_OUTPUT"
          fi

  test:
    needs: plan
    if: needs.plan.outputs.rows != '[]'
    name: Test (${{ matrix.name }})
    runs-on: ${{ matrix.runs_on }}
    strategy:
      fail-fast: false
      matrix:
        include: ${{ fromJSON(needs.plan.outputs.rows) }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Install Aster
        run: cargo install aster --locked
      - uses: actions/setup-go@v5
        if: contains(matrix.toolchains, 'go')
      - uses: actions/setup-node@v4
        if: contains(matrix.toolchains, 'nodejs')
      - name: Test lane
        run: |
          if [ "${{ github.event_name }}" = schedule ]; then
            aster affected test --all --lane "${{ matrix.lane }}"
          else
            aster affected test --base=origin/${{ github.base_ref || 'main' }} --related --lane "${{ matrix.lane }}"
          fi
```

The plan and test jobs pass the same flags, so a row exists exactly when its
lane has something to run. A lane-specific setup step reads a `matrix` field
(`if: matrix.browser`) rather than comparing lane names.
