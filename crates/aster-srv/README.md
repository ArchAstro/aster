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
| `GET /healthz` | The process is up. |
| `GET /readyz` | The database answers. |

Rows are kept for the retention period (30 days by default). Day partitions
past it are dropped every six hours.

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

## Reading results

These routes take the same bearer token as uploads, and a token reads only its
own repository. `repo=<owner/name>` is optional and, when sent, must be that
repository (`403` otherwise). Errors: `400` for a bad parameter or cursor,
`401` for a missing or invalid token, `403` for another repository's data,
`404` for an unknown test or run. Timestamps are RFC 3339. State is as of the
last classifier pass, up to five minutes old.

Lists are paged with `limit` and an opaque `cursor`; pass the previous page's
`next_cursor` until it is `null`.

`GET /v1/test-results/tests?state=flaky|broken|healthy&limit=100&cursor=`
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
`failures` are the objects above plus `run_id`, `job` and `lane`, ordered by
`run_id` then `test_id`:

```json
{
  "github_run": "18233412",
  "runs": [
    { "run_id": "…", "job": "test-ci", "lane": "platform", "sha": "…",
      "branch": "main", "pr": 16161, "attempt": 1, "received_at": "…" }
  ],
  "failures": [ { "run_id": "…", "job": "test-ci", "lane": "platform", "test_id": "…", "…": "as above" } ],
  "next_cursor": null
}
```

`aster results failures --github-run <id>` fetches every page and prints the
merged JSON.

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
| `ASTER_SRV_STATIC_TOKENS` | unset | `owner/name=token` pairs accepted instead of OIDC tokens. Development only. |

Logs are one JSON object per line, with `severity` and `message`.

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
