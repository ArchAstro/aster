# aster-srv

Aster's server. It is one service for what Aster runs need beyond a single
machine. Its first feature is CI test results; others, such as a shared build
cache, live beside it under their own routes and Postgres schema.

## Test results

`aster --test-results` records every test a run executes. With
`ASTER_SRV_URL` set, Aster uploads the results after the run; `aster results
upload <file>` sends a file by hand. In GitHub Actions, Aster then adds the
run's failures to the job's step summary and annotates them (set
`ASTER_GITHUB_SUMMARY=0` to skip); `aster results summary <file> [--github]`
does the same by hand. Failures are labelled `new`, `broken` or `known flake`
from this server's state, looked up with the same credentials.

| Route | |
|---|---|
| `POST /v1/test-results/runs` | Store one gzip NDJSON results stream. `201` stored, `200` already stored (same `run_id`), `401` bad token, `403` repository not allowed or results for another repository, `413` too large. |
| `GET /v1/test-results/tests` | Tests of the caller's repository, flakiest first. |
| `GET /v1/test-results/tests/:test_id` | One test's state and history. |
| `GET /v1/test-results/runs/:run_id/failures` | The failing tests of a run, with their state. |
| `GET /v1/test-results/failures?github_run=` | The failing tests of every Aster run in one GitHub Actions run. |
| `GET /v1/test-results/quarantine` | The caller's quarantine list and its version. See [Quarantine](#quarantine). |
| `GET /v1/test-results/durations?target=&day=` | Each test file's median duration, for splitting a partitioned suite. |
| `GET /healthz` | The process is up. Answers while migrations wait for locks. |
| `GET /readyz` | The database answers. |

Rows are kept for the retention period (30 days by default). Day partitions
past it are dropped every six hours, with the older quarantine log rows and
webhook deliveries.

## Flaky and broken tests

Every five minutes one replica (an advisory lock) classifies the runs received
since its last pass and keeps `state` (`healthy`, `flaky` or `broken`),
`flake_score`, `last_flake_at`, `broken_since_sha` and `broken_failure_hash` on
each test. Evidence goes to `test_flake_events`, a day-partitioned table kept
for the retention period. Passes are idempotent: events are keyed by (repo,
test, kind, run), and state is re-derived from events and results, never
incremented. A first pass over existing data works through it in six-hour
pieces.

A test's result in a run is its final attempt. Skipped tests and cached
targets are ignored (a cached target replays old results). Runs are ordered by
`received_at`.

| Event | Rule |
|---|---|
| definite, `retry` | The final attempt passed and an earlier attempt of the test in the same run failed or errored. |
| definite, `input_hash` | In an earlier run within the retention period, with the same target `input_hash`, the test's outcome (pass against fail or error) differs from this run's. Compared with the nearest earlier run of that hash, on any branch. |
| likely, `next_main_run` | On the default branch the test passed and its previous default-branch run failed, with any hash, unless the run already has a definite event for the test or that failure ended a broken streak (so fixing a broken test is not flaking). Whether the change between the two runs touched the test is not knowable here. |

`partial`-confidence targets count toward definite events, since a flip is a
flip however it was captured. They are left out of likely events and broken
streaks, which judge a failure that console parsing may have missed or
misattributed.

**Broken**: the test's last `ASTER_SRV_BROKEN_RUNS` (at least 3, default 3)
default-branch runs all failed with the same non-null `failure_hash`.
`broken_since_sha` is the sha of the first run of that whole streak. The next
pass that finds a pass (or a different failure) clears it. A broken test is
never flaky, even with a positive score.

**Score**: `flake_score` is 3 per definite event plus 1 per likely event in
the last 7 days. `state` is `broken`, else `flaky` when the score is positive,
else `healthy`. `last_flake_at` is the latest event kept.

The default branch is `ASTER_SRV_DEFAULT_BRANCH` (default `main`); a run's
branch matches it as `main` or `refs/heads/main`.

`test_results.failure_clusters` is a view over the retained failures: per
(repo, `failure_hash`) the `failures` and distinct `tests` counts, `first_seen`
and `last_seen` days, and an example message and test.

## Quarantine

At the end of each pass, in the same transaction and as of the same clock,
the classifier updates `test_results.quarantine`, in this order:

1. A quarantined test that is now **broken** leaves: broken tests are never
   quarantined.
2. An entry older than `ASTER_SRV_QUARANTINE_DAYS` (default 30, at most the
   retention period) **expires**.
3. A test whose final results since it entered end in
   `ASTER_SRV_QUARANTINE_RELEASE_PASSES` (default 20) consecutive passes is
   **released**. Any failure resets the count.
4. A test that is not broken **enters** once definite flake events from
   `ASTER_SRV_QUARANTINE_RUNS` (default 2) different runs fall within the last
   7 days. Only events after it last left count, so a released or expired
   test is judged afresh. `0` turns automatic quarantine off.

Every change is a row in `test_results.quarantine_log` (`quarantined`,
`released`, `expired` or `broken`, with details), kept for the retention
period. A repository's newest log id is its list's `version`.

`GET /v1/test-results/quarantine` (the upload token; `repo=` must match it)
returns:

```json
{"repo": "ArchAstro/firstlanding", "version": 312, "generated_at": "…",
 "truncated": false,
 "tests": [{"test_id": "c81e…", "target": "//services/platform:test-ci",
            "file": "test/platform/agents_test.exs", "suite": "Platform.AgentsTest",
            "name": "test creates an agent", "since": "…", "expires_at": "…",
            "runs": 2}]}
```

It lists at most 10,000 tests (`truncated` says when there were more), which
only lets more tests fail. `GET /v1/test-results/tests?state=quarantined`
filters the test list the same way. How Aster applies the list, and the
repository's own `aster-quarantine.toml`, are in
[docs/test-tracking.md](../../docs/test-tracking.md#5-quarantine).

## Webhooks

`ASTER_SRV_WEBHOOKS` is a JSON array of endpoints, each with the events it
wants:

```json
[{"name": "flake-bot", "url": "https://hooks.example.com/aster",
  "secret": "at-least-16-characters", "events": ["test.quarantined", "test.released"]}]
```

| Event | When |
|---|---|
| `test.quarantined` | A test entered quarantine. `detail`: `runs`, `events`, `expires_at`. |
| `test.released` | It passed enough times in a row. `detail`: `entered_at`, `passes`. |
| `test.expired` | Its entry reached the longest stay. `detail`: `entered_at`. |
| `test.broken` | A test became broken, quarantined or not. `detail`: `since_sha`, `failure_hash`. |

Each delivery is a `POST` of

```json
{"event": "test.quarantined", "occurred_at": "…", "repo": "ArchAstro/firstlanding",
 "test": {"test_id": "…", "target": "…", "file": "…", "suite": "…", "name": "…"},
 "detail": {…}}
```

with `X-Aster-Event`, `X-Aster-Delivery` (the same on every retry, so a
receiver can drop repeats) and `X-Aster-Signature: sha256=<hex HMAC-SHA256 of
the body, keyed by the secret>`. Check the signature over the raw body before
parsing it.

The classifier records deliveries in `webhooks.deliveries` in the transaction
that makes the change, so nothing is announced that did not happen and
nothing that happened is lost. A sender on every replica claims due rows
every 15 seconds (`FOR UPDATE SKIP LOCKED`) and posts them with a 10-second
timeout. A 2xx delivers; anything else is retried after 30 seconds, doubling
up to 6 hours, and abandoned after 12 attempts (about 14.5 hours). A slow or
failing endpoint holds up only its own deliveries. Endpoints must be `https://`
(`http://` only for localhost). Unsubscribed events, and all events with no
endpoints configured, record nothing. Rows are kept for the retention period.
## Reading results

These routes take the same bearer token as uploads, and a token reads only its
own repository. `repo=<owner/name>` is optional and, when sent, must be that
repository (`403` otherwise). Errors: `400` for a bad parameter or cursor,
`401` for a missing or invalid token, `403` for another repository's data,
`404` for an unknown test or run. Timestamps are RFC 3339. State is as of the
last classifier pass, up to five minutes old.

Lists are paged with `limit` and an opaque `cursor`; pass the previous page's
`next_cursor` until it is `null`.

`GET /v1/test-results/tests?state=flaky|broken|healthy|quarantined&limit=100&cursor=`
(`limit` 1 to 500, default 100), sorted by `flake_score` descending, then
`test_id`:

```json
{
  "repo": "ArchAstro/firstlanding",
  "tests": [
    {
      "test_id": "…", "target": "//services/platform:test-ci",
      "file": "test/a_test.exs", "suite": "Platform.Test", "name": "test a",
      "state": "flaky", "flake_score": 4,
      "last_flake_at": "2026-10-14T01:02:03.000Z",
      "broken_since_sha": null, "broken_failure_hash": null,
      "first_seen": "…", "last_seen": "…"
    }
  ],
  "next_cursor": null
}
```

`GET /v1/test-results/tests/:test_id?limit=100&cursor=` returns the same test
object plus its retained history (up to `limit`, 1 to 500, newest first, later
attempts first):

```json
{
  "repo": "…",
  "test": { "…": "as above" },
  "history": [
    {
      "run_id": "…", "sha": "…", "branch": "main", "attempt": 2,
      "outcome": "pass", "duration_ms": 7, "failure_hash": null,
      "observed_at": "2026-10-14T01:02:03.000Z"
    }
  ],
  "next_cursor": "…"
}
```

`GET /v1/test-results/runs/:run_id/failures?limit=1000&cursor=` (the run id
percent-encoded, as it holds `/`) lists the tests that failed or errored on
any attempt of the run, by `test_id`. Merge-queue reports can skip those with
`passed_on_retry`:

```json
{
  "run": { "run_id": "…", "repo": "…", "sha": "…", "branch": "main", "pr": 16161, "received_at": "…" },
  "failures": [
    {
      "test_id": "…", "name": "…", "file": "…", "suite": "…", "target": "…",
      "state": "flaky",
      "flake_score": 3, "last_flake_at": "…", "broken_since_sha": null,
      "failure_hash": "f3a9", "message": "(scrubbed)",
      "final_outcome": "pass", "passed_on_retry": true,
      "attempts": [
        { "attempt": 1, "outcome": "fail", "duration_ms": 5 },
        { "attempt": 2, "outcome": "pass", "duration_ms": 7 }
      ]
    }
  ],
  "next_cursor": null
}
```

`state` here is `flaky`, `broken`, or `new` for a failure the classifier
judges healthy (or has not seen yet).

`GET /v1/test-results/failures?github_run=<id>[&attempt=<n>][&limit=&cursor=]`
answers the same question for a caller that knows only the GitHub Actions run
id (one run holds an Aster run per job or lane). It covers the caller's
repository's runs within the retention period whose `run_id` starts
`gh:<repo>:<github run>:` (and `<attempt>:`), and is `404` when none match.
`failures` are the objects above plus `run_id`, `job`, `lane` and `runner`,
ordered by `run_id` then `test_id`. `runner` is the runner's name
(`RUNNER_NAME`), which is how to match a run to a job from GitHub's jobs API
(its `runner_name`): `job` is the workflow's job key, not the job's display
name.

```json
{
  "github_run": "18233412",
  "runs": [
    { "run_id": "…", "job": "test-ci", "lane": "platform",
      "runner": "blacksmith-1", "sha": "…",
      "branch": "main", "pr": 16161, "attempt": 1, "received_at": "…" }
  ],
  "failures": [ { "run_id": "…", "job": "test-ci", "lane": "platform", "runner": "blacksmith-1", "test_id": "…", "…": "as above" } ],
  "next_cursor": null
}
```

`aster results failures --github-run <id>` fetches every page and prints the
merged JSON.

### Durations

`GET /v1/test-results/durations?target=<address>&target=…&day=<YYYY-MM-DD>`
returns each test file's median duration over the targets' recent runs, for
splitting a partitioned suite (`MIX_TEST_PARTITION=K mix test --partitions N`)
by time. The answer for a set of targets and a UTC `day` depends on nothing
else: it covers runs received in the 7 days before `day` began (less
15 minutes for uploads still committing), and a `day` that has not begun is
`409`. Per file, the first-attempt durations of its tests are summed in each
of the latest 20 uncached, completely captured runs of each target, and the
median of those sums is `p50_ms`. 1 to 32 targets; `files` is sorted and
empty without history.

```json
{
  "repo": "ArchAstro/firstlanding",
  "targets": ["//services/platform-test-shard-1:test-ci", "…"],
  "day": "2026-10-15",
  "files": [ { "file": "test/platform/agents_test.exs", "p50_ms": 18234, "runs": 60 } ]
}
```

Partitions never ask the server while they run: one shard that could not
reach it would split differently from the rest, and a file would run on two
shards or none. Instead `aster results durations` asks for every partitioned
suite in the workspace and writes `aster-durations.json` at its root, to be
committed. A target that runs a partition of a suite reads its suite there
and runs the files it gets from a longest-first split, so every shard of a
commit splits the same way. Without the file or the suite in it, or with
`ASTER_BALANCE_PARTITIONS=off`, Mix splits as usual. Files new since the
refresh count as the suite's median. Refresh it on a schedule, for example:

```yaml
on:
  schedule: [{ cron: "0 6 * * 1" }]
permissions: { id-token: write, contents: write, pull-requests: write }
jobs:
  durations:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: aster results durations
        env: { ASTER_SRV_URL: https://aster-srv.example.com }
      - run: |
          git diff --quiet aster-durations.json && exit 0
          git switch -c aster-durations-$(date +%F)
          git commit -am "chore: refresh aster-durations.json"
          git push -u origin HEAD && gh pr create --fill
        env: { GH_TOKEN: "${{ github.token }}" }
```

The shards' tests are recorded under the project whose suite it is (the
directory the command `cd`s to), so a test's id does not depend on the shard
that ran it.

## Authentication

CI authenticates with its GitHub Actions OIDC token for the `aster-srv`
audience. The workflow job needs:

```yaml
permissions:
  id-token: write
env:
  ASTER_SRV_URL: https://aster-srv.example.com
```

A token is accepted when GitHub signed it, it is unexpired, its audience is
`ASTER_SRV_AUDIENCE`, and its repository is on `ASTER_SRV_REPOSITORIES`. A
repository can upload only its own results.

### Behind Google IAP

To keep the server off the open internet, put it behind Identity-Aware Proxy
and let CI pass IAP with Workload Identity Federation:

1. CI exchanges its GitHub OIDC token for a Google service account
   (`google-github-actions/auth`) that holds `roles/iap.httpsResourceAccessor`
   on the server's host only.
2. That account signs a short-lived JWT with the IAM Credentials API
   (`aud: https://<host>/*`), which IAP accepts.
3. `aster results upload` sends it in `Proxy-Authorization` from
   `ASTER_SRV_PROXY_TOKEN`; `Authorization` keeps the GitHub OIDC token, so
   the server still knows which repository is uploading.

## Dashboard

`/ui` is a server-rendered, read-only HTML dashboard for people, not CI. It
needs no JavaScript.

| Path | Shows |
|---|---|
| `/ui` | Flaky leaderboard (flake score, last flake) and broken tests |
| `/ui/tests/:test_id?repo=owner/name` | One test: state, a pass/fail strip (oldest to newest) and recent results with failure messages |
| `/ui/clusters` | Failures of the last 7 days grouped by failure hash |
| `/ui/quarantine` | The quarantine list, its rules and its recent changes |
| `/ui/cache?repo=owner/name&target=<substring>` | The artifact cache: manifest and blob counts and sizes, the last write and its writer (ref, workflow, run linked to GitHub Actions, commit), 14 days of lookup hits, misses and hit rate, and the entries (target, key, size, output files, hits, last hit, created, last used, writer run) 50 per page with a filter and a next-page link. Says so when `ASTER_SRV_CACHE_BUCKET` is not set. See [Artifact cache](#artifact-cache). |

`?repo=owner/name` picks the repository; without it the one with the newest
run is shown. Any repository on `ASTER_SRV_REPOSITORIES` can be viewed.

Viewers are identified by the Google IAP assertion
(`x-goog-iap-jwt-assertion`): ES256, signed by Google's published IAP keys,
unexpired, issued by `https://cloud.google.com/iap`, addressed to
`ASTER_SRV_IAP_AUDIENCE`. The viewer is the assertion's `email`. The pod must
accept traffic only through IAP, since the header is trusted once verified.

`ASTER_SRV_IAP_AUDIENCE` is `/projects/PROJECT_NUMBER/global/backendServices/ID`.
GKE ingress assigns the backend service ID, so a value ending in `/`, such as
`/projects/123456/global/backendServices/`, matches any backend service of
that project. Without it the dashboard is off (`/ui` is a 404).

`ASTER_SRV_UI_EMAIL_DOMAINS` limits who may view: `example.com` for that
domain, `*.example.com` for its subdomains. Unset, anyone who passes IAP may
view, including service accounts such as the CI uploader; set it to keep
those out (list `*.gserviceaccount.com` to let them in).

For local development, `ASTER_SRV_UI_DEV_EMAIL=you@example.com` with no IAP
audience makes the dashboard trust that viewer without any sign-in. Never set
it where the server is reachable by others; the server logs a warning at
startup.

Pages are sent with a strict `Content-Security-Policy`, `nosniff`,
`Referrer-Policy: same-origin` and `X-Frame-Options: DENY`.

## Configuration

| Variable | Default | |
|---|---|---|
| `ASTER_SRV_DATABASE_URL` | required | Postgres URL. Its `sslmode` decides TLS: `disable`, `prefer` (default) or `require`. As with libpq's `require`, the connection is encrypted but the server certificate is not verified, which suits Cloud SQL's private IP. |
| `ASTER_SRV_REPOSITORIES` | required | Comma-separated `owner/name` or `owner/*`. |
| `ASTER_SRV_LISTEN` | `0.0.0.0:8080` | |
| `ASTER_SRV_AUDIENCE` | `aster-srv` | The OIDC audience. |
| `ASTER_SRV_ISSUER` | GitHub Actions | |
| `ASTER_SRV_RETENTION_DAYS` | `30` | |
| `ASTER_SRV_DEFAULT_BRANCH` | `main` | The branch whose runs decide broken and likely-flaky tests. |
| `ASTER_SRV_BROKEN_RUNS` | `3` | Consecutive default-branch runs failing the same way that make a test broken; at least 3. |
| `ASTER_SRV_MAX_BODY_BYTES` | 64 MiB | Largest upload, before and after decompression. |
| `ASTER_SRV_RAW_BUCKET` | unset | A GCS bucket that keeps each upload as sent, using the pod's Workload Identity. Give the bucket a lifecycle rule that deletes objects after the retention period. |
| `ASTER_SRV_IAP_AUDIENCE` | unset | The Google IAP audience the dashboard accepts, exact or a prefix ending in `/`. Unset turns the dashboard off. See [Dashboard](#dashboard). |
| `ASTER_SRV_UI_EMAIL_DOMAINS` | unset | Comma-separated domains (`example.com`, `*.example.com`) whose emails may view the dashboard. Unset allows anyone who passes IAP. |
| `ASTER_SRV_UI_DEV_EMAIL` | unset | The viewer to trust when no IAP audience is set. Development only. |
| `ASTER_SRV_QUARANTINE_RUNS` | `2` | Different runs with a definite flake in 7 days that quarantine a test; `0` turns automatic quarantine off. |
| `ASTER_SRV_QUARANTINE_RELEASE_PASSES` | `20` | Consecutive passes that release a quarantined test. |
| `ASTER_SRV_QUARANTINE_DAYS` | `30` | The longest a test stays quarantined; at most `ASTER_SRV_RETENTION_DAYS`. |
| `ASTER_SRV_WEBHOOKS` | unset | Webhook endpoints, as JSON. See [Webhooks](#webhooks). |
| `ASTER_SRV_STATIC_TOKENS` | unset | `owner/name=token` pairs accepted instead of OIDC tokens. Development only. They may write the artifact cache. |
| `ASTER_SRV_CACHE_BUCKET` | unset | The GCS bucket for cache blobs (`cas/<sha256>`). Unset disables the cache. The pod's Workload Identity needs `storage.objects.*` on the bucket and `iam.serviceAccounts.signBlob` on itself. |
| `ASTER_SRV_CACHE_SIGNER` | unset | The pod's Workload Identity service account email. Required with `ASTER_SRV_CACHE_BUCKET`; blob URLs are signed as this account through `signBlob`. |
| `ASTER_SRV_CACHE_DAYS` | `14` | Drop manifests unused this long, then blobs no remaining manifest names, and uncommitted uploads older than this. |
| `ASTER_SRV_CACHE_WRITERS` | `*=refs/heads/<default branch>` | Comma-separated `owner/name=ref` or `*=ref` patterns that may upload and commit. A pattern ending in `*` matches that prefix. The pattern is compared with the token's `ref` claim. |

Logs are one JSON object per line, with `severity` and `message`.

## Runbook

| Symptom | Check | Do |
|---|---|---|
| Uploads fail | `/readyz`; logs with `"severity":"ERROR"` | Uploads never fail CI, so results are only missing. Fix the database or the allowlist (`403`). |
| Classification stalls | `SELECT watermark FROM test_results.classifier_state` should be minutes old; `classifier pass failed` in the logs | A pass is safe to rerun; restarting a pod starts one at once. |
| A real regression is being let through | `/ui/quarantine`, the test's page | In CI set `ASTER_QUARANTINE=off` for the job to fail it at once. On the server, delete its row: `DELETE FROM test_results.quarantine WHERE repo = '…' AND test_id = '…'` (it can re-enter on new flakes after now). Set `ASTER_SRV_QUARANTINE_RUNS=0` to stop new entries. |
| Too many or too few tests quarantined | `SELECT action, count(*) FROM test_results.quarantine_log WHERE at > now() - interval '1 day' GROUP BY 1` | Tune `ASTER_SRV_QUARANTINE_RUNS` and `ASTER_SRV_QUARANTINE_RELEASE_PASSES`; they apply from the next pass. |
| Webhooks not arriving | `SELECT endpoint, event, attempts, last_status, last_error FROM webhooks.deliveries WHERE delivered_at IS NULL ORDER BY id DESC LIMIT 20` | Abandoned rows have `abandoned_at`; to resend, `UPDATE webhooks.deliveries SET abandoned_at = NULL, attempts = 0, next_attempt_at = now() WHERE id = …`. |
| A deploy waits at startup | `migration waiting for table locks` in the logs | Expected while the old release holds the tables. The server listens meanwhile: `/healthz` is 200, `/readyz` and every other route are 503, and migrations retry every few seconds until they apply. `/readyz`'s `migration.state` is `waiting_for_locks` for this case. |
| A deploy never becomes ready | `/readyz` body: `migration.state`, `waiting_seconds`, `error`; `migrations failed; retrying` at `"severity":"ERROR"` | `failing` means waiting will not fix it (a bad migration, credentials, an unreachable database): the pod stays live and retries forever, so alert on unready time rather than restarts, fix the cause or roll back. `waiting_for_locks` past 10 minutes also logs at `ERROR`: find the holder in `pg_locks`. |

## Image

`ghcr.io/archastro/aster-srv:<aster version>` is published with each Aster
release. Build it yourself from the repository root:

```console
docker build -f crates/aster-srv/Dockerfile -t aster-srv .
```

## Tests

```console
docker run -d --rm -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16
ASTER_SRV_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
  cargo test -p aster-srv
```

Without `ASTER_SRV_TEST_DATABASE_URL` the database tests skip. Set
`ASTER_SRV_TEST_TLS_DATABASE_URL` to a TLS-enabled Postgres URL with
`sslmode=require` to also check that connections are encrypted.

## Artifact cache

The server is the gateway to the shared artifact cache. Manifests are rows in
`cache.manifests`, keyed by repository and task key. Blobs are objects
`cas/<sha256>` in `ASTER_SRV_CACHE_BUCKET`, named in `cache.blobs`. Clients
move blob bytes directly to and from GCS through V4 signed URLs that last 15
minutes and sign only the `host` header, so the URL alone is enough. The pod
signs them with IAM Credentials `signBlob` as its own service account
(`ASTER_SRV_CACHE_SIGNER`); no key file is stored.

| Route | |
|---|---|
| `POST /v1/cache/lookup` | `{repo, keys[]}` → `{hits: {key: {manifest, blobs}}}`. `manifest` is base64. `blobs` maps each sha256 the manifest names to a signed GET URL, and only when that object exists. An unknown key is absent from `hits`, not an error. A lookup updates `last_used_at`. |
| `POST /v1/cache/upload` | `{repo, digests[]}` → `{missing: {sha256: signed PUT URL}}` for blobs not yet in the bucket. Writers only. |
| `POST /v1/cache/commit` | `{repo, key, manifest}` stores the manifest once every blob it names exists, and records the writer's `ref`, `run_id`, `sha` and `workflow_ref` claims (null for a static token). A blob uploaded through a signed URL is hashed on its first commit; bytes that do not match their sha256 are deleted and reported missing. `201` stored, `409` the key is already stored, `422` a named blob is missing or corrupt. Writers only. |
| `GET /v1/cache/summary?repo=<owner/name>` | `{repo, manifests, manifest_bytes, blobs, blob_bytes, last_write_at, last_writer: {ref, run_id, sha, workflow} or null, lookups: [{day, hits, misses}]}`. `lookups` are the last 14 UTC days that had a lookup, newest first. `blobs` and `blob_bytes` are global: blobs are content-addressed and shared across repositories. |
| `GET /v1/cache/entries?repo=<owner/name>&limit=<1..200, default 50>&before=<cursor>&target=<substring>` | `{entries: [{key, target, size_bytes, output_files, exit_code, duration_ms, aster_version, created_at, last_used_at, hits, last_hit_at, writer: {ref, run_id, sha, workflow}}], next}` ordered by `created_at` descending, then key. `next` is an opaque cursor to pass as `before`, or null on the last page. `target` is a case-insensitive substring filter, applied while reading at most 2000 rows per request; a page can then hold fewer than `limit` entries and still carry a `next`. A manifest that does not decode is listed with an empty `target` and zeroed fields. |

A lookup counts one hit per stored key (`hits`, `last_hit_at` on the manifest)
and adds the request's hits and misses to today's row in
`cache.lookup_stats`. Counting is best effort: a failure is logged and the
lookup still answers. Keys repeated in one request count once.

Reads accept any authenticated CI token for the repository, and an IAP viewer
whose email `ASTER_SRV_UI_EMAIL_DOMAINS` allows. The two `GET` browsing routes
answer `401` without an identity and `403` for a viewer outside those domains,
a repository the server does not serve, or a CI token for another repository.
People browse the same data on the dashboard at `/ui/cache`. There is no
separate allow-list for browsing: whoever may view the dashboard may browse
the cache. Writes accept a static
development token, or a CI token whose `ref` matches `ASTER_SRV_CACHE_WRITERS`
(default the repository's default branch, `refs/heads/main`). A pull-request
ref is refused with `403`, so a branch cannot poison the cache.

Every six hours one replica drops manifests unused for `ASTER_SRV_CACHE_DAYS`
(default 14) and blobs no remaining manifest names, and deletes those objects.
It also deletes `cas/` objects no commit recorded once they are older than
the same cutoff: signed PUTs whose commit never came. The pod needs
`storage.objects.list` on the bucket for this.
