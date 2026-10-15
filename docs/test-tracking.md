# Test tracking

Aster can record every test a CI run executes, retry only the tests that
failed, tell flaky tests from broken ones across runs, and keep known flakes
from failing the build. This page covers the whole loop:

```text
CI runner                                   aster-srv (Postgres)
---------                                   --------------------
aster --test-results --retry-failed  ──▶  POST /v1/test-results/runs
  capture each runner's report              classifier, every 5 min:
  retry failing tests                         flaky / broken / healthy
  quarantine list at start  ◀──────────────   quarantine list + webhooks
  step summary + annotations                /ui dashboard (behind IAP)
```

Everything on the CI side works without a server. A server adds history,
labels (`new`, `broken`, `known flake`) and the automatic quarantine list.

## 1. Capture

`--test-results[=PATH]` (or `ASTER_TEST_RESULTS=PATH`, or `=1` for a temp
path) reads each runner's own report and writes one NDJSON file per Aster
invocation: a run record, a record per target, and one per test attempt.

| Runner | How Aster reads it | Confidence |
| --- | --- | --- |
| `mix test` (ExUnit) | Aster's formatter, loaded beside `ExUnit.CLIFormatter`, checked against ExUnit's summary | complete |
| `go test` | `-json`, rendered back to plain `go test` output in the log | complete |
| `pytest` | `--junitxml` | complete |
| Vitest, `bun test` | JUnit reporter beside the default one | complete |
| Jest | `--json` | complete |
| `node --test` | Aster's reporter | complete |
| `cargo nextest run` | JUnit profile | complete |
| `cargo test` | console `test x ... ok/FAILED` lines | partial |
| Gradle, Maven | existing `build/test-results` and `target/surefire-reports` files | complete |

`partial` means some tests may be missing. Partial results are still uploaded
and classified, but they never let a quarantined failure pass.

## 2. Retry

`--retry-failed[=N]` (or `ASTER_RETRY_FAILED=N`) reruns only the failing
tests, with the runner's own selection flags, and records each attempt. See
[Retrying failed tests](../README.md#retrying-failed-tests) for the runner
table and `retry_setup`.

## 3. Upload and labels

| Variable | |
| --- | --- |
| `ASTER_SRV_URL` | The server. Set, Aster uploads after the run and fetches the quarantine list before it. |
| `ASTER_SRV_TOKEN` | A static token. Without it, GitHub Actions' OIDC token is used (the job needs `permissions: id-token: write`). |
| `ASTER_SRV_AUDIENCE` | The OIDC audience; default `aster-srv`. |
| `ASTER_SRV_PROXY_TOKEN` | Sent as `Proxy-Authorization` when the server sits behind an identity-aware proxy. |
| `ASTER_GITHUB_SUMMARY` | `0` stops the step summary and annotations in GitHub Actions. |
| `ASTER_QUARANTINE` | `off` stops quarantine; failures fail as usual. |

An upload or lookup that fails never fails the build. `aster results upload
<file>`, `aster results summary <file> [--github]` and `aster results failures
--github-run <id>` do the same steps by hand.

A GitHub Actions job:

```yaml
permissions:
  id-token: write
  contents: read
steps:
  - run: aster affected test --retry-failed --test-results
    env:
      ASTER_SRV_URL: https://aster-srv.example.com
```

The step summary lists each failing test as `new`, `broken` (with the commit
it broke at), `known flake`, `quarantined` or, without a server, `failed`,
and each gets an annotation on its file.

## 4. Flaky and broken

The server classifies every run. A test is a **definite flake** when it failed
and then passed on a retry in one run, or when two runs with the same target
`input_hash` (the same inputs) disagree. It is **broken** when its last 3
default-branch runs failed the same way; broken tests are never quarantined.
The details are in the [server README](../crates/aster-srv/README.md#flaky-and-broken-tests).

## 5. Quarantine

A quarantined test still runs and still uploads. When it fails, its target
may report `passed (quarantined: N)` instead of failing, and its dependents
run.

### What makes a failure pass

All of these, after retries:

1. The target's results are `complete`.
2. Every test whose last attempt failed is quarantined, and there is at least
   one.
3. The command exited with its runner's "tests failed" code: ExUnit 2,
   nextest 100, pytest, `go test`, Jest, Vitest, `bun test` and `node --test`
   1. A crash, compile error, timeout or signal exits otherwise and still
   fails. Gradle, Maven and `cargo test` use one code for test failures and
   compile errors, so their targets never pass this way.
4. `ASTER_QUARANTINE` is not `off`.

A target that runs several commands, or whose results are not captured (no
`--test-results` and no `--retry-failed`), is never judged. When some failures
are quarantined but the target still fails, its log says why.

Each failure let through gets a GitHub `::warning` annotation and a row in the
step summary; `--json` lists them under `tests.quarantined`; the results file
marks those attempts `"quarantined": true`. A pass that needed the quarantine
is not cached.

### Where the list comes from

At start Aster prints the list it will use, for example:

```text
[aster] Quarantine: 4 from aster-srv (version 312), 1 from aster-quarantine.toml
```

**The server** quarantines a test that is not broken once definite flakes
from 2 different runs fall within 7 days. It releases the test after 20
consecutive passes, when it breaks, or after 30 days; a released test starts
afresh. The list is fetched at start and cached in `.aster/quarantine.json`.
If the server cannot be reached, a cache younger than 24 hours stands in;
otherwise only the repository file applies, so nothing extra is let through.

**The repository** can quarantine tests itself in `aster-quarantine.toml` at
the workspace root, reviewed like any change. Every entry needs a `reason` and
an `expires` date, and stops applying after that date (with a warning). An
entry names a test by `id` (shown on the dashboard), or by `name` with
optional `file`, `suite` and `target` (a project or one of its targets) to
narrow it:

```toml
# aster-quarantine.toml
[[test]]
name = "test creates an agent"
file = "test/platform/agents_test.exs"
target = "//services/platform"
reason = "DBConnection.OwnershipError under load; tracked in #4812"
expires = 2026-11-30

[[test]]
id = "c81e728d9d4c2f636f067f89cc14862c0f5b6a1e9e9e0f4c6b1a2d3e4f5a6b7c"
reason = "Times out on the 4-core runners"
expires = 2026-11-15
```

A malformed file stops the run with an error naming the entry.

## 6. Notifications

The server can POST to webhooks when a test is quarantined, released, expires
from quarantine, or breaks. See
[Webhooks](../crates/aster-srv/README.md#webhooks).

## 7. Self-hosting

aster-srv is one binary (and the image
`ghcr.io/archastro/aster-srv:<aster version>`) that needs Postgres. The
[server README](../crates/aster-srv/README.md) covers configuration,
authentication, the dashboard and operating it.
