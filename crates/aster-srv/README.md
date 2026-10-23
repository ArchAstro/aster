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

An event belongs to the run that **failed** (`run_id`; `prev_run_id` is the
passing run it was compared with) and is observed when that run was received.
One failure is one event, however many passes it is compared with: a pass, a
failure and a pass on one input hash score 3, not 6.

| Event | Rule |
|---|---|
| definite, `retry` | The final attempt passed and an earlier attempt of the test in the same run failed or errored. |
| definite, `input_hash` | In an earlier run within the retention period, with the same target `input_hash`, the test's outcome (pass against fail or error) differs from this run's. Compared with the nearest earlier run of that hash, on any branch. The event is for whichever of the two runs failed. |
| likely, `next_main_run` | On the default branch the test passed and its previous default-branch run failed, with any hash, unless that failure ended a broken streak (so fixing a broken test is not flaking). The event is for the failed run; a definite event for that run replaces it. Whether the change between the two runs touched the test is not knowable here. |

**Incidents**: a target run whose tests failed together failed for one shared
cause (a crashed shard, a dead database, a test-harness process that died),
not because each test is flaky. A non-cached target run is an incident when
20 or more of its tests failed (in any attempt), or 5 or more that are at least
half of its tests, or 10 or more of its final failures share one
`failure_hash`. It is one row in `test_results.target_incidents` (run, target,
input hash, failures, tests, its most shared failure hash and how many share
it), and its failures make no flake events of either kind. They still count
toward broken streaks: a change that breaks many tests at once is broken.

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
   `ASTER_SRV_QUARANTINE_RUNS` (default 2) different failing runs fall within
   the last 7 days. Only events after it last left count, so a released or
   expired test is judged afresh. `0` turns automatic quarantine off. A run
   holding definite events of more than `ASTER_SRV_QUARANTINE_RUN_CAP`
   (default 10) tests in those days counts toward none of them: many tests
   flaking in one run point at the run. The cap counts the events found so far: a run
   whose flakes are found over several passes can cross it after it has
   already helped quarantine a test, and that entry stays until it is
   released or expires.

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

`/v1/*` authenticates every request itself (a CI OIDC token, or a person's
token, below), so it can be served without IAP in front of it; `/`, `/ui`,
`/cli-auth` and `/healthz` are for browsers and stay behind IAP. A deployment
that still puts IAP in front of `/v1` can let CI pass it with Workload
Identity Federation. The server ignores `Proxy-Authorization`:

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
| `ASTER_SRV_MAX_LARGE_BODIES` | `8` | Large request bodies (a cache commit's manifest, a test-results upload) one replica reads at once, 1 to 1024. Past it a request gets `503` with `Retry-After: 1`. |
| `ASTER_SRV_RAW_BUCKET` | unset | A GCS bucket that keeps each upload as sent, using the pod's Workload Identity. Give the bucket a lifecycle rule that deletes objects after the retention period. |
| `ASTER_SRV_IAP_AUDIENCE` | unset | The Google IAP audience the dashboard accepts: exactly `/projects/NUMBER/global/backendServices/ID`. A project-wide prefix is refused at startup, since it would accept an assertion for any other backend service of the project. Unset turns the dashboard off. See [Dashboard](#dashboard). |
| `ASTER_SRV_UI_EMAIL_DOMAINS` | unset | Comma-separated domains (`example.com`, `*.example.com`) whose emails may view the dashboard. Unset allows anyone who passes IAP. |
| `ASTER_SRV_UI_DEV_EMAIL` | unset | The viewer to trust when no IAP audience is set. Development only. |
| `ASTER_SRV_QUARANTINE_RUNS` | `2` | Different failing runs with a definite flake in 7 days that quarantine a test; `0` turns automatic quarantine off. |
| `ASTER_SRV_QUARANTINE_RELEASE_PASSES` | `20` | Consecutive passes that release a quarantined test. |
| `ASTER_SRV_QUARANTINE_DAYS` | `30` | The longest a test stays quarantined; at most `ASTER_SRV_RETENTION_DAYS`. |
| `ASTER_SRV_QUARANTINE_RUN_CAP` | `10` | A run with definite flakes of more tests than this in 7 days counts toward quarantining none of them. |
| `ASTER_SRV_WEBHOOKS` | unset | Webhook endpoints, as JSON. See [Webhooks](#webhooks). |
| `ASTER_SRV_STATIC_TOKENS` | unset | `owner/name=token` pairs accepted instead of OIDC tokens. Development only. They may write the artifact cache. |
| `ASTER_SRV_CACHE_BUCKET` | unset | The GCS bucket for cache blobs (`cas/<sha256>`). Unset disables the cache. The pod's Workload Identity needs `storage.objects.*` on the bucket and `iam.serviceAccounts.signBlob` on itself. |
| `ASTER_SRV_CACHE_SIGNER` | unset | The pod's Workload Identity service account email. Required with `ASTER_SRV_CACHE_BUCKET`; blob URLs are signed as this account through `signBlob`. |
| `ASTER_SRV_CACHE_DAYS` | `14` | Drop manifests unused this long, then blobs no remaining manifest names, and uncommitted uploads older than this. |
| `ASTER_SRV_CACHE_WRITERS` | `*=refs/heads/<default branch>` | Comma-separated `owner/name=ref` or `*=ref` patterns that may upload and commit. A pattern ending in `*` matches that prefix. The pattern is compared with the token's `ref` claim. |
| `ASTER_SRV_CACHE_WRITER_WORKFLOWS` | unset | Comma-separated prefixes of the token's `job_workflow_ref` claim (`owner/repo/.github/workflows/ci.yml@`; each must contain `@`). When set, a CI token writes the cache or uploads test results only from a matching workflow, in addition to the ref rule. Required whenever `ASTER_SRV_CACHE_WRITERS` is set (startup fails otherwise unless `ASTER_SRV_ALLOW_ANY_WRITER_WORKFLOW=1`, a development escape hatch). Static tokens are exempt. |
| `ASTER_SRV_REPOSITORY_OWNER_IDS` | unset | Comma-separated `owner/name=id` or `owner/*=id`. A token for a matching repository must carry that `repository_owner_id` claim, so a repository recreated under the same name by someone else is refused with `403`. |
| `ASTER_SRV_DEV_BLOB_ROUTE` | unset | `1` makes the in-memory blob store answer `PUT\|GET /<bucket>/cas/<sha>`, the test stand-in for a signed GCS URL. Tests and local development only; no deploy sets it and the route does not exist without it. |
| `ASTER_SRV_PUBLIC_URL` | unset | The https URL people reach the server at (`https://aster-srv.example.com`); the `iss` and `aud` of person tokens. Required for [Laptop sign-in](#laptop-sign-in). With an https URL every response carries `Strict-Transport-Security: max-age=31536000; includeSubDomains`. An `http://` URL is refused at startup unless `ASTER_SRV_ALLOW_HTTP_PUBLIC_URL=1` (development and tests only). |
| `ASTER_SRV_CLI_TOKEN_KEYS` | unset | `kid:secret` pairs, comma-separated. A secret is hex (64+ digits) or base64 (43+ characters) for at least 32 random bytes, as `openssl rand -base64 32` makes; passphrases, short keys and patterned keys (one byte repeated, a repeated block, a counting run, fewer than 16 distinct bytes) stop startup. The first signs person tokens and form MACs; all verify. Setting it also requires `ASTER_SRV_IAP_AUDIENCE` and `ASTER_SRV_UI_EMAIL_DOMAINS`, and forbids `ASTER_SRV_UI_DEV_EMAIL`. Keep it in a secret store. |
| `ASTER_SRV_PERSON_TOKENS` | `on` | `off` is the kill switch for laptop sign-in: every person token is refused and `/cli-auth` and `/v1/auth/token` answer `404`, whatever else is set. CI tokens are unaffected. Any value but `on` or `off` stops startup. See [Rotating the signing keys](#rotating-the-signing-keys). |
| `ASTER_SRV_CLI_TOKEN_TTL` | `43200` | Seconds a person token lives, 60 to 43200 (12 hours; verification enforces the same cap). |
| `ASTER_SRV_API_ADDR` | unset | A second listener, such as `0.0.0.0:8081`, that serves only `/v1/*`. It refuses to start unless `ASTER_SRV_STATIC_TOKENS` and `ASTER_SRV_DEV_BLOB_ROUTE` are unset, `ASTER_SRV_REPOSITORY_OWNER_IDS` pins every `ASTER_SRV_REPOSITORIES` entry, `ASTER_SRV_CACHE_WRITER_WORKFLOWS` is set and `ASTER_SRV_TRUSTED_PROXY_HOPS` is at least 1. See [Hardening](#hardening-the-v1-channel). |
| `ASTER_SRV_TRUSTED_PROXY_HOPS` | `1` | `X-Forwarded-For` entries from the right that infrastructure appended. See [Client IP](#client-ip-and-rate-limits). |
| `ASTER_SRV_RATE_TOKEN_IP_PER_MIN` | `10` | `POST /v1/auth/token` per client IP per minute, per replica. |
| `ASTER_SRV_RATE_API_IP_PER_MIN` | `1200` | All `/v1/*` per client IP per minute, per replica. |
| `ASTER_SRV_RATE_API_SUBJECT_PER_MIN` | `3000` | Authenticated `/v1/*` per subject (a person, or a CI repository) per minute, per replica. |
| `ASTER_SRV_HEADER_TIMEOUT_SECS` | `20` | Time to send request headers; also how long an idle connection lives. |
| `ASTER_SRV_BODY_TIMEOUT_SECS` | `60` | Time for a request body to arrive in full (`408`). |
| `ASTER_SRV_UNAUTH_BODY_TIMEOUT_SECS` | `8` | Time for the body of an unauthenticated route (`POST /v1/auth/token`, `POST /cli-auth`, 8 KiB at most) to arrive (`408`); never more than `ASTER_SRV_BODY_TIMEOUT_SECS`, which the authenticated uploads keep. |
| `ASTER_SRV_MAX_CONNECTIONS_PER_IP` | `128` | Requests in flight from one client IP (resolved as in [Client IP](#client-ip-and-rate-limits); IPv6 by /64), per replica; more get `429`. With `ASTER_SRV_TRUSTED_PROXY_HOPS=0` the peer is the client, so it also caps that peer's open connections (extra ones are closed unserved). |
| `ASTER_SRV_LOG_SAMPLE_FIRST` | `5` | Failure audit lines (`auth_failure`, `rate_limited`, a failed `token_exchange`) logged per event, reason, route and client IP in each window; the rest are counted and summarised. |
| `ASTER_SRV_LOG_SAMPLE_WINDOW_SECS` | `60` | The sampling window, and how often `audit_summary` lines are written. |
| `ASTER_SRV_HANDLER_TIMEOUT_SECS` | `120` | Time from the end of the headers to the reply, body included (`504`). |
| `ASTER_SRV_MAX_CONNECTIONS` | `1024` | Open connections per listener; more wait in the accept queue. |
| `ASTER_SRV_DB_WAIT_SECS` | `5` | How long a request waits for a database connection before failing. |

Logs are one JSON object per line, with `severity` and `message`.

## Hardening the /v1 channel

`/v1/*` is the channel that does not sit behind IAP: callers carry their own
bearer tokens (CI OIDC tokens, person tokens) and `POST /v1/auth/token` carries
none. Everything below applies to it.

**Separate listener.** With `ASTER_SRV_API_ADDR` set, a second port serves only
`/v1/*`. `/ui`, `/cli-auth`, `/healthz`, `/readyz`, `/metrics` and anything else
answer `404` there, whatever the load balancer's URL map says, so a routing
mistake cannot expose the dashboard or sign-in pages without IAP. The main
port is unchanged and still answers `/v1/*`. The listener has no IAP in front of
it, so it starts only with the settings that make a CI token mean what it
says: no static tokens, no `ASTER_SRV_DEV_BLOB_ROUTE`, an owner-id pin
(`ASTER_SRV_REPOSITORY_OWNER_IDS`) covering every `ASTER_SRV_REPOSITORIES`
entry (`owner/*=id` covers `owner/*` and `owner/name`; `owner/name=id` covers
only that repository), a writer-workflow list
(`ASTER_SRV_CACHE_WRITER_WORKFLOWS`; `ASTER_SRV_ALLOW_ANY_WRITER_WORKFLOW`
does not apply) and `ASTER_SRV_TRUSTED_PROXY_HOPS` of at least 1. Startup
fails naming every missing one. With the listener on, a token for a
repository no pin covers is refused too. Give the non-IAP backend service
the API port; keep IAP on the main port. The API port's backend health check
uses `GET /v1/healthz`, which answers `200 ok` and nothing else, is not
authenticated (a health check carries no credentials) and reveals only that
the process is up. It is not served on the main port.

**Client IP and rate limits.** Limits are in-memory token buckets, per
replica: with N replicas an attacker gets N times the rate. Cloud Armor on the
load balancer is the global, cross-replica limit; these limits protect each
replica and add per-subject limits Cloud Armor cannot. Over a limit the
server answers `429` with `Retry-After`.

| Limit | Key | Default |
|---|---|---|
| `POST /v1/auth/token` | client IP | 10 per minute |
| all `/v1/*` | client IP | 1200 per minute |
| authenticated `/v1/*` | person `sub`, or CI repository | 3000 per minute |
| requests in flight | client IP | 128 (more get `429`) |
| concurrent exchanges | the replica | 4 (more get `503`, `Retry-After: 1`) |
| concurrent large bodies (cache commit, test-results upload) | the replica | 8, `ASTER_SRV_MAX_LARGE_BODIES` (more get `503`, `Retry-After: 1`) |

Google's load balancer sets `X-Forwarded-For` to `<client-supplied>, <client
IP>, <load balancer IP>`. Everything left of the last two entries is under the
caller's control, so the server counts from the right, skips
`ASTER_SRV_TRUSTED_PROXY_HOPS` entries (default 1, the balancer's own
address) and takes the next: the right-most address that is not ours. With too
few entries, or an unparsable one, it uses the TCP peer; with hops 0 it
ignores the header. IPv6 clients share a limit per /64. This holds only while
pods are reachable through the balancer alone; a caller reaching a pod
directly could write the whole header (network policy must prevent it).

**Writers are checked before the body.** The route gate holds a CI token to
the route's writer rule before reading any body: for `/v1/cache/upload` and
`/v1/cache/commit` the cache rule (ref from `ASTER_SRV_CACHE_WRITERS`, a
`push` event, a `job_workflow_ref` prefix from
`ASTER_SRV_CACHE_WRITER_WORKFLOWS`), so a pull-request job's token gets `403`
without the server reading up to 64 MiB first. `POST /v1/test-results/runs`
still accepts pull-request runs (a retry that passes there is the flake
signal) and refuses `pull_request_target` and foreign workflows; only the
large-body limit applies to it beyond that. The repository allowlist and
owner-id pin are applied when the token is verified, which is also before any
body.

Each limiter tracks at most 100,000 keys. Past that, idle keys are dropped
(at most once a second) and a newcomer shares one of 256 overflow buckets
chosen by a keyed hash, so a flood of minted client IPs does not take a real
client's whole allowance, and it never touches the per-subject limiter, which
has its own table.

**Size, time and connection limits.** `POST /cli-auth` and `/v1/auth/token`
take at most 8 KiB, uploads and cache writes `ASTER_SRV_MAX_BODY_BYTES`, and
every other route takes no body (`413`, decided from `Content-Length` before
reading and while reading chunked bodies). Header, body and whole-request
times are capped (`408`, `504`); the unauthenticated bodies get the short
`ASTER_SRV_UNAUTH_BODY_TIMEOUT_SECS`. The header timeout also closes an idle
keep-alive connection. Each listener serves at most
`ASTER_SRV_MAX_CONNECTIONS` connections and one client IP holds at most
`ASTER_SRV_MAX_CONNECTIONS_PER_IP` requests open, so a few addresses
trickling bodies cannot take every slot. Database connections are capped too:
the shared pool waits `ASTER_SRV_DB_WAIT_SECS` and fails, and
`POST /v1/auth/token` has its own 2-connection pool so a flood of sign-in
attempts cannot take connections from CI commits and uploads.

**Audit log.** Structured lines with `"audit": true` and an `event`:
`token_mint` (a code issued at `/cli-auth`), `token_exchange` (`outcome` and
`reason`), `token_revoke` (`audit::token_revoke`, called by the revoke
endpoint), `auth_failure` (every 401 and 403: `route`, `client_ip`, `subject`
when known, `reason`) and `rate_limited`. The three failure events are
sampled so a flood of 401s does not become a flood of log lines: the first
`ASTER_SRV_LOG_SAMPLE_FIRST` (5) per event, reason, route and client IP in each
`ASTER_SRV_LOG_SAMPLE_WINDOW_SECS` (60) window are logged, then one
`audit_summary` line (`of`, `reason`, `route`, `client_ip`, `suppressed`)
when the window ends. The counters below stay exact, and `token_mint`, a
successful `token_exchange` and `token_revoke` are never sampled. Alert on
the counters, not on line counts. Tokens, codes, verifiers and IAP
assertions are never fields, and the logger redacts credential-named fields
and JWT-shaped text anyway.

**Metrics.** `GET /metrics` on the main port, for callers that reach the pod
directly (a request through the load balancer carries `X-Forwarded-For` and
IAP's header and gets `404`); never on the API port.
`aster_srv_auth_failures_total{route,reason}`,
`aster_srv_person_tokens_issued_total`, `aster_srv_rate_limited_total{route,scope}`.
Alert on a spike in failures, in PromQL
`sum(rate(aster_srv_auth_failures_total[5m])) > 1` for 10 minutes, or in Cloud
Monitoring with a log-based metric on
`jsonPayload.audit=true AND jsonPayload.event="auth_failure"`.

## Runbook

| Symptom | Check | Do |
|---|---|---|
| Uploads fail | `/readyz`; logs with `"severity":"ERROR"` | Uploads never fail CI, so results are only missing. Fix the database or the allowlist (`403`). |
| Classification stalls | `SELECT watermark FROM test_results.classifier_state` should be minutes old; `classifier pass failed` in the logs | A pass is safe to rerun; restarting a pod starts one at once. |
| A real regression is being let through | `/ui/quarantine`, the test's page | In CI set `ASTER_QUARANTINE=off` for the job to fail it at once. On the server, delete its row: `DELETE FROM test_results.quarantine WHERE repo = '…' AND test_id = '…'` (it can re-enter on new flakes after now). Set `ASTER_SRV_QUARANTINE_RUNS=0` to stop new entries. |
| Too many or too few tests quarantined | `SELECT action, count(*) FROM test_results.quarantine_log WHERE at > now() - interval '1 day' GROUP BY 1` | Tune `ASTER_SRV_QUARANTINE_RUNS` and `ASTER_SRV_QUARANTINE_RELEASE_PASSES`; they apply from the next pass. |
| Many tests flaked at once | `SELECT run_id, target, failures, tests, failure_hash, shared FROM test_results.target_incidents ORDER BY received_at DESC LIMIT 20` | An incident is one shared cause in one target run: find it from the run's log and the shared failure hash. Its tests are not scored or quarantined. |
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
| `POST /v1/cache/lookup` | `{repo, keys[]}` → `{hits: {key: {manifest, blobs}}}`. `manifest` is base64. `blobs` maps each sha256 the manifest names to a signed GET URL, and only when that object exists. An unknown key is absent from `hits`, not an error. A key whose blobs the server could not resolve (over the blob cap, past the time budget, or GCS or IAM failed or was busy) is also absent, so the client treats it as a miss and rebuilds. See [Lookup bounds](#lookup-bounds). A lookup updates `last_used_at`. |
| `POST /v1/cache/upload` | `{repo, digests[]}` → `{missing: {sha256: signed PUT URL}}` for blobs not yet in the bucket. Writers only. |
| `POST /v1/cache/commit` | `{repo, key, manifest}` stores the manifest once every blob it names exists, and records the writer's `ref`, `run_id`, `sha` and `workflow_ref` claims (null for a static token). A blob uploaded through a signed URL is hashed on its first commit; bytes that do not match their sha256 are deleted and reported missing. `201` stored, `409` the key is already stored, `422` a named blob is missing or corrupt. Writers only. |
| `GET /v1/cache/summary?repo=<owner/name>` | `{repo, manifests, manifest_bytes, blobs, blob_bytes, last_write_at, last_writer: {ref, run_id, sha, workflow} or null, lookups: [{day, hits, misses}]}`. `lookups` are the last 14 UTC days that had a lookup, newest first. `blobs` and `blob_bytes` are global: blobs are content-addressed and shared across repositories. |
| `GET /v1/cache/entries?repo=<owner/name>&limit=<1..200, default 50>&before=<cursor>&target=<substring>` | `{entries: [{key, target, size_bytes, output_files, exit_code, duration_ms, aster_version, created_at, last_used_at, hits, last_hit_at, writer: {ref, run_id, sha, workflow}}], next}` ordered by `created_at` descending, then key. `next` is an opaque cursor to pass as `before`, or null on the last page. `target` is a case-insensitive literal substring filter (`%`, `_` and `\` match themselves), applied in SQL, so a page holds `limit` entries whenever more match. Commit stores `target`, `output_files`, `exit_code`, `duration_ms` and `aster_version` beside the manifest, so paging never reads manifest bytes. A manifest that does not decode is listed with an empty `target` and zeroed fields. |

A lookup counts one hit per stored key (`hits`, `last_hit_at` on the manifest)
and adds the request's hits and misses to today's row in
`cache.lookup_stats`. Counting is best effort: a failure is logged and the
lookup still answers. Keys repeated in one request count once.

### Lookup bounds

One lookup's GCS and IAM work is bounded, so a wide or cold lookup answers
inside the client's 5s request timeout with fewer hits rather than timing out
and losing them all:

- **Blob cap.** One response names at most 4096 distinct blobs
  (`MAX_LOOKUP_BLOBS`). Entries are admitted in the order the request lists
  them; an entry whose blobs would push the total over the cap is absent
  from `hits` (a miss) and is not counted.
- **Time budget.** Blobs are resolved 16 at a time per request, for at most
  3 seconds. An entry with a blob still unresolved then is a miss.
- **Remembered answers.** A positive HEAD is remembered for an hour (blobs
  are content-addressed and a blob a stored manifest names is not deleted by
  retention); a miss is never remembered. A signed GET URL is reused while
  at least 5 of its 15 minutes remain. Concurrent lookups of one blob share
  one HEAD and one signature. This memory is per replica.
- **IAM `signBlob` limit.** Per replica, at most 16 signatures in flight and
  a token bucket of 200 per second with a burst of 512. A signature that
  would wait more than 2 seconds fails with "signing is busy" and its entry
  is a miss. HEADs are capped at 64 in flight per replica.

Every route needs a bearer token; an IAP assertion alone is never accepted
here, so a request with neither answers `401`. Reads accept any authenticated
CI token for the repository, and a person's token (see
[Laptop sign-in](#laptop-sign-in)) whose email `ASTER_SRV_UI_EMAIL_DOMAINS`
allows, for any repository on `ASTER_SRV_REPOSITORIES`. The two `GET` browsing
routes answer `401` without a token and `403` for a person outside those
domains, a repository the server does not serve, or a CI token for another
repository. People browse the same data on the dashboard at `/ui/cache`.
Writes accept a static development token, or a CI token whose `ref` matches
`ASTER_SRV_CACHE_WRITERS` (default the repository's default branch,
`refs/heads/main`) and, when `ASTER_SRV_CACHE_WRITER_WORKFLOWS` is set, whose
`job_workflow_ref` (else `workflow_ref`) starts with one of its prefixes. A
pull-request ref is refused with `403`, so a branch cannot poison the cache,
and so is a person's token. Cache writes also need `event_name` `push`
(pull requests, `pull_request_target`, `workflow_run`, merge queues, schedules
and manual dispatches are `403` whatever ref they carry). Test-result uploads
follow the workflow rule and refuse `pull_request_target`, but accept pull
requests, since a retry that passes there is the flake signal.

### Route table and who may call what

`routes::ROUTES` is the one list of routes. Each has an auth class and the
router checks it, from the headers, before it reads a body or runs a handler;
a `/v1/` path the table does not list is `401` without a valid token.

| Class | Routes | Who |
|---|---|---|
| `Public` | `GET /healthz`, `GET /readyz`, `POST /v1/auth/token` | anyone; the exchange needs a valid code |
| `Iap` | `/cli-auth`, `/ui/*` | a person behind IAP |
| `CiOrPerson` | `GET /v1/test-results/{tests,tests/:id,failures,quarantine,clusters,durations,runs/:id/failures}`, `GET /v1/cache/{summary,entries}`, `POST /v1/cache/lookup` | a CI token for its own repository, or a person's token for any served repository named with `?repo=` (read-only; a person's lookups do not count hits) |
| `CiWriter` | `POST /v1/test-results/runs`, `POST /v1/cache/{upload,commit}` | a CI token only, then the writer rules |

A lookup or upload request body is capped at 1 MiB and read only after the
token is verified; a commit may be as large as `ASTER_SRV_MAX_BODY_BYTES`.

## Laptop sign-in

`aster cache login` signs a person in so a laptop can read the cache. It is
an authorization-code flow with PKCE against the CLI's loopback port.

| Route | |
|---|---|
| `GET /cli-auth?port=&challenge=&state=` | Behind IAP. Needs a valid IAP assertion from an allowed domain. Renders "Authorize aster CLI as `email`?" with a form that carries a MAC over the email, challenge, port, state and an expiry (10 minutes), so a page rendered for one person cannot be submitted for another. `port` is 1024 to 65535, `challenge` a 43-character base64url SHA-256, `state` 1 to 128 base64url characters. |
| `POST /cli-auth` | Behind IAP. Re-verifies the assertion and the MAC, stores the SHA-256 of a new 256-bit code (`auth.cli_codes`, 60 seconds), and answers `302 http://127.0.0.1:<port>/callback?code=&state=`: loopback only, never another host. |
| `POST /v1/auth/token` | `{code, verifier, port}` → `{token, expires_at, email}`. Not behind IAP: the code and the PKCE verifier are the credential. One `DELETE ... RETURNING` spends the code, so it works once even across replicas. Every refusal (unknown, spent or expired code, wrong verifier or port, malformed body) is the same `400 {"error":"invalid_grant"}`. |

A person's token is an HS256 JWT signed by this server: `iss` is
`ASTER_SRV_PUBLIC_URL`, `aud` is the same URL, `sub` is IAP's stable subject
(`accounts.google.com:<id>`), `email` is the email IAP verified, `scope` is
`read` (it covers the cache and every dashboard dataset), with `iat`, `nbf`, `exp` (at most 12 hours after `iat`) and
`jti`, and a `kid` header. Verification pins HS256, requires every claim, and
allows 30 seconds of clock skew. Tokens issued before `sub`, `nbf` and `email`
were required are refused; those people sign in once more. There
are no refresh tokens. `/v1/*` reads the token's `alg`: HS256 goes to these
keys only and RS256 to the GitHub OIDC verifier only; any other algorithm is
`401`. The domain rule is applied again on every request, so narrowing
`ASTER_SRV_UI_EMAIL_DOMAINS` takes effect on tokens already issued.

Sign-in is off, failing closed, unless both `ASTER_SRV_CLI_TOKEN_KEYS` and
`ASTER_SRV_PUBLIC_URL` are set and IAP (`ASTER_SRV_IAP_AUDIENCE`) is
configured: `/cli-auth` and `/v1/auth/token` answer `404` and person tokens are
refused.

Every six hours one replica drops manifests unused for `ASTER_SRV_CACHE_DAYS`
(default 14) and blobs no remaining manifest names, and deletes those objects.
It also deletes `cas/` objects no commit recorded once they are older than
the same cutoff: signed PUTs whose commit never came. The pod needs
`storage.objects.list` on the bucket for this. The same pass drops
`cache.lookup_stats` days older than the longer of 30 days and
`ASTER_SRV_CACHE_DAYS`, and sign-in codes nobody redeemed (`auth.cli_codes`).

A lookup answers before its day's `cache.lookup_stats` row is written: that
write runs afterwards with a 3 second limit, and a failure is logged and the
request's counts are lost. The counts are lookup requests (each distinct key
counts once), not successful restores.

### Rotating the signing keys

The deploying repository sets `ASTER_SRV_CLI_TOKEN_KEYS` from its secret store;
these are the steps on the server's side.

`ASTER_SRV_CLI_TOKEN_KEYS` is a comma-separated list of `kid:secret` pairs,
such as `k2:<new secret>,k1:<old secret>`. The first pair signs new person
tokens (its `kid` goes in the token header) and the `/cli-auth` form MACs;
every pair verifies. Key ids use letters, digits, `-`, `_` and `.`, and may
not repeat.

A secret is at least 32 random bytes, written as base64 (43+ characters) or
hex (64+ digits): `openssl rand -base64 32` or `openssl rand -hex 32`.
Passphrases, short keys and patterned keys stop startup.

Planned rotation, with nobody signed out:

1. Generate a new secret and put its pair first: `k2:<new>,k1:<old>`. Deploy.
   New tokens are signed with `k2`; tokens signed with `k1` still verify.
2. Wait a little over 12 hours after every replica runs the new list (the
   longest a token lives, whatever `ASTER_SRV_CLI_TOKEN_TTL` says, plus 30
   seconds of clock skew), so every `k1` token has expired.
3. Drop the old pair: `k2:<new>`. Deploy.

Emergency rotation, when a secret leaked: replace the whole list with one new
pair, `k3:<new>`, and deploy. Every token signed with an older key is refused
at once, so everyone signs in again with `aster cache login`; a `/cli-auth`
page open during the deploy fails its MAC and has to be reloaded.

Kill switch: `ASTER_SRV_PERSON_TOKENS=off` refuses every person token and
answers `404` on `/cli-auth` and `/v1/auth/token`, whatever the keys say. CI
tokens are unaffected. Set it back to `on` (or unset it) to re-enable sign-in;
tokens still within their lifetime and signed by a listed key work again.

### Revoking a departed person's tokens

Removing someone from IAP stops new sign-ins, but a token they already hold
works until it expires (up to 12 hours). `aster cache logout` revokes a
person's own tokens; an operator does the same for someone else by writing
their row in `auth.revocations` directly. There is no admin route for this.

1. Remove their IAP access (or narrow `ASTER_SRV_UI_EMAIL_DOMAINS`), so they
   cannot sign in again.
2. Find the person's subject, IAP's stable `sub` (`accounts.google.com:<id>`),
   not their email. Every sign-in writes an audit line with both:
   `jsonPayload.audit=true AND jsonPayload.event="token_mint" AND
   jsonPayload.email="ada@example.com"` in Cloud Logging, and read
   `jsonPayload.subject`. (`auth.cli_codes` does not help: a code row is
   deleted when it is redeemed and is only good for 60 seconds.) Revoke
   every distinct subject the email shows.
3. Connect to the server's database with `psql` as a role that can write the
   `auth` schema (for Cloud SQL, through the Cloud SQL Auth Proxy or however
   your deployment reaches the database) and run, with the subject from step 2:

<!-- revoke-sql:start -->
```sql
\set subject 'accounts.google.com:1234567890'
INSERT INTO auth.revocations (subject, not_before) VALUES (:'subject', now())
ON CONFLICT (subject) DO UPDATE SET not_before = greatest(auth.revocations.not_before, now());
```
<!-- revoke-sql:end -->

Every replica checks `auth.revocations` on each request, so the person's
tokens issued up to that second get `401` from then on; nothing needs a
restart. This write is not audited by the server: record who ran it
and why. The `tests/operator_revoke.rs` test runs this snippet from this file.
