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

A runner behind a program listed in `[affected.related] wrappers` (`mise exec
go -- bun test`, `scripts/with-rust-toolchain cargo test`) is read through the
wrapper: Aster plans the report on the command the wrapper runs, then writes
the wrapper back in front, so the flags land after the runner and the console
output is unchanged. A pytest `generated xml file` line or a Jest `Test results
written to` line that names Aster's report directory is omitted from the log, so
a cache replay does not keep a path to a deleted directory. Pytest's
`collected N items` header is left as the runner printed it: Aster does not pass
`-q`, and `--junitxml` alone does not drop that header. The same reading applies
to `--retry-failed`. A runner
behind a program that is not listed records no tests; `aster check` reports
such a target as `uncaptured-test-results`.

A package script that chains commands (`npm`/`pnpm`, `a && b && c`, also
through the scripts it calls) is run step by step: the steps `--related`
expands it into, each started with `pnpm exec` or `npm exec -c` as the script
would. Every test runner step has its own report in its own directory and the
results of all steps merge into the target's. Other steps (`tsc`, `eslint`, a
generator) run as written. A failing test is retried by rerunning only its
step's runner, narrowed to that test, and the chain continues; a step whose
only failures are quarantined continues too. A runner that crashes before
writing a report makes the results partial, so the target fails and nothing is
excused. A chain with a `cd`, a `pre`/`post` hook, a cycle, another shell
operator, or a runner step Aster cannot plan is not captured.

Where Aster runs the runner itself the results are `complete`: a `node --test`
command (or a script that is only that), each `node --test` step of a chain,
and `cargo test` when every test binary's `test result:` summary is there and
adds up to the tests read. `partial` remains only where Aster cannot know:

| Plan | Why partial |
| --- | --- |
| `node --test` mentioned in a shell chain or script file | the command may run tests another way too |
| `mix test` in a project that sets its own ExUnit formatters | only failures in the console output are read |
| Vitest writing its own JUnit report to a path Aster cannot resolve | the report is the command's, and the path is an expansion |
| `cargo test` whose summaries do not add up | a binary died or a line was broken |
| a declared `libtest` report | saved console output |

`aster check` reports each of these as `unquarantinable-test-results`, and a
target that cannot be retried as `unretryable-test-target` (`cargo test`
without `--no-fail-fast`, `-x`, `--bail`, Gradle and Maven, scripts).

`partial` means some tests may be missing. Partial results are still uploaded
and classified, but they never let a quarantined failure pass.

`cargo nextest run` writes JUnit only when the repository's nextest config
gives the profile a `junit.path`; without it nothing is recorded.

### Vitest commands that already write a report, and shell expansions

Aster adds a JUnit reporter to a Vitest command. A command that already
passes `--reporter=junit` with a file (`--outputFile.junit=<path>`,
`--outputFile.junit <path>`, or a plain `--outputFile=<path>` when no other
file reporter such as `json` is listed) is not given a second one: Vitest
turns two `--outputFile.junit` flags into an array and its JUnit reporter
throws before any test runs. Aster leaves the command unchanged and reads the
file it names, relative to `--root` or the working directory. Under
`--retry-failed` the rerun writes that file again and is read the same way.

A `sh -c`/`bash -c` one-liner may use `$VAR` and `${VAR:-default}` in the
runner's arguments (`vitest run ${E2E_FILE:-e2e}`); they are kept exactly as
written. A report path that is an expansion is worked out from Aster's own
environment, which the command inherits: `${OUT:-/tmp/r.xml}` is
`/tmp/r.xml` when `OUT` is unset. Aster does not see variables the one-liner
sets itself (`OUT=r.xml; vitest ... --outputFile.junit=$OUT`) or `~`; for those
it leaves the command alone and records no tests. Declare the report (below)
or use a literal path. Aster still does not recognise a runner behind an
expansion (`$RUNNER run`), command substitution (`$(...)`, backticks) or an
unbalanced `${`.

When a target Aster planned to capture ends with 0 test cases, the run
prints `[aster] Warning: no test results were captured for <target>`: the
report was empty or missing, so quarantine and flake tracking cannot apply
to that run.

### Declared reports

Aster recognises a runner in the target's command, directly or as the last
command of a `sh -c` one-liner. A command that runs a script (`bash
tools/cargo-test.sh`, `node scripts/related-tests.mjs`) hides its runner, and
the target records no tests. Such a target declares its report instead:

```toml
[targets.test]
command = "bash tools/cargo-test.sh"

[targets.test.report]
format = "junit"   # junit, go-json, jest-json or libtest
files = ["target/nextest/ci/junit.xml"]
```

The command runs as written, with `ASTER_TEST_REPORT_DIR` set to an empty
directory outside the checkout. Without `files`, Aster reads every file the
command writes there (`go test -json ./... > "$ASTER_TEST_REPORT_DIR/go.json"`).
With `files`, it reads those globs under the project directory. Either way only
files written while the command ran count, so an old report is never read as
this run's. Prefer `ASTER_TEST_REPORT_DIR`: a fixed path can be read by
another target that writes the same file, and when `--only-affected-files`
splits a target into several commands only the last command's file is read
(the results are then partial). `files` are relative to the project
directory, not a target's `working_dir`. A declared report wins over
recognising the command. `libtest` (saved `cargo test` console output) is
partial; the others are complete, and the capture is recorded as
`declared-<format>`.

`aster check` reports every test target that has no capturable results and no
declared report (`uncaptured-test-results`), with the reason, because such a
target cannot be retried or quarantined. `[check] allow_no_test_results` lists
targets that run no per-test runner at all.

A declared report on a script never lets a quarantined failure pass: Aster does
not see the runner, so it cannot tell a test failure from a failed step of the
script by the exit code. A declared report on a command whose runner Aster
recognises (one that names it, not through a shell)
(`node --test ...` or `vitest run ...` that writes its own JUnit or JSON file)
is retried: the rerun is built from the runner and each attempt is read from
the declared report. On a script (`bash tools/run.sh`) there is no runner to
rerun, and the script's exit code is not read as a runner's, so it is neither
retried nor quarantined.

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
| `ASTER_SRV_PROXY_TOKEN` | Sent as `Proxy-Authorization` when set, for a deployment that still keeps an identity-aware proxy in front of `/v1`. The server ignores it. A laptop does not set it: see [Reading the cache from a laptop](#reading-the-cache-from-a-laptop). |
| `ASTER_GITHUB_SUMMARY` | `0` stops the step summary and annotations in GitHub Actions. |
| `ASTER_QUARANTINE` | `off` stops quarantine; failures fail as usual. |
| `ASTER_BALANCE_PARTITIONS` | `off` keeps Mix's own split for `mix test --partitions` shards. |

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


## Reading the cache from a laptop

CI authenticates every request with its GitHub OIDC token. A laptop or a devbox
has none, so it signs in as a person, once per machine (a devbox does not
share the laptop's sign-in):

```console
export ASTER_SRV_URL=https://aster-srv.example.com
aster srv login
```

`ASTER_SRV_URL` must be `https://`; plain `http://` is accepted only for
`127.0.0.1`, `::1` and `localhost`, because the code, verifier and token would
cross the network in the clear.

1. `aster srv login` makes a PKCE verifier and a short **user code**
   (`ABCD-EFGH`), prints the code, and opens
   `$ASTER_SRV_URL/cli-auth` in your browser.
2. You sign in there with your company Google account (the server's
   identity-aware proxy asks). The page shows the user code, your address and
   browser. Click **Authorize** only if the code matches your terminal.
3. The server redirects the browser to `http://127.0.0.1:<port>/callback`
   with a single-use code (valid 60 seconds). Aster trades the code, plus the
   PKCE verifier and user code, for a token.
4. The token is stored in `$XDG_CONFIG_HOME/aster/cache-tokens/<server host>.json`
   (else `~/.config/aster/...`; `%APPDATA%\aster\...` on Windows), mode `0600` in
   a `0700` directory (on Windows an ACL naming only you; if Aster cannot set
   it, it does not store the token). Aster ignores a token file that group or
   others can read and tells you to log in again. The token is never printed,
   and it lives outside the repository. It lasts 12 hours by default
   (`ASTER_SRV_CLI_TOKEN_TTL` on the server); then run `aster srv login`
   again.

The next `aster run` with `ASTER_SRV_URL` set sends the token as a bearer
token and restores a cached target. Nothing else needs configuring.

### Over SSH or on a devbox

A browser on your laptop cannot reach `127.0.0.1` on the remote machine, so
Aster uses a pasted code there. It picks that flow by itself when
`SSH_CONNECTION`, `SSH_CLIENT` or `SSH_TTY` is set, when a Linux shell has
neither `DISPLAY` nor `WAYLAND_DISPLAY`, or when the system browser fails to
start. `aster srv login --no-browser` asks for it explicitly:

```console
$ aster srv login
This looks like a remote shell, so Aster will ask for a code instead of a redirect.
Open this URL in a browser on any computer and sign in:
https://aster-srv.example.com/cli-auth?mode=copy&challenge=...&state=...&user_code=K7QM2XWD
Confirm only if the page shows this code: K7QM-2XWD
Request id: Xb3kP9aQ
Paste the code the page shows, then press Enter: <paste here>
Signed in as ada@example.com. Token stored at ...
```

1. Open the printed URL in a browser on any computer. Confirm the page shows
   the same user code and your own address.
2. Click **Authorize**. The page shows a one-time code (never a token) and
   the request id.
3. Paste the code into the terminal. It works once and expires after 60
   seconds, like the loopback code; the PKCE verifier stays on the remote
   machine, so the code alone signs nobody in.

`--loopback` forces the browser redirect, for example with
`ssh -R PORT:127.0.0.1:PORT` forwarding (add `--no-browser` to print the URL
instead of opening one).

### Signing out

`aster srv logout` deletes the stored token and asks the server to refuse
every token issued to you before now, on every machine. If the server cannot
be reached the local file is still deleted and the command fails, so run it
again later. Operators can turn person sign-in off for everyone with
`ASTER_SRV_PERSON_TOKENS=off` on the server: `/cli-auth` answers 404 and every
person token is refused.

Signed out, or with an expired or refused token, `aster run` prints one line
(`not signed in; run aster srv login`) and builds from the local cache. It
does not fail. If the server has not enabled sign-in, `aster srv login`
says the server refused it.

A person's token reads and never writes: it cannot upload or commit (the
server answers `403`), cannot upload test results, and Aster never asks to
write with it. Entries CI writes carry the CI runner's platform in their key,
so a macOS laptop mostly misses entries written on Linux CI.

### Reading the dashboard's data

The same token reads every dataset the `/ui` pages show, as JSON, for any
repository the server serves. `aster results` and `aster cache stats` print
it; the repository defaults to
`GITHUB_REPOSITORY`, else the checkout's `origin` (`--repo owner/name`
overrides).

| `/ui` page | Command | Route |
|---|---|---|
| Cache | `aster cache stats` | `GET /v1/cache/summary`, `/v1/cache/entries?target=` |
| Flaky tests | `aster results flaky` | `GET /v1/test-results/tests?state=flaky` |
| Flaky tests (broken list) | `aster results broken` | `GET /v1/test-results/tests?state=broken` |
| Failure clusters | `aster results clusters` | `GET /v1/test-results/clusters` |
| Quarantine | `aster results quarantine` | `GET /v1/test-results/quarantine` |
| A test | `aster results test <id>` | `GET /v1/test-results/tests/<id>` |
| Anything else | `aster srv get '<path>'` | any `GET /v1/...` route, such as `/v1/test-results/durations?...` or `/v1/test-results/runs/<id>/failures` |

The pre-0.25 spellings (`aster cache login`/`logout`, `aster srv flaky`,
`broken`, `quarantine`, `clusters`, `test`, `cache`) still work, are hidden
from `--help`, and print the new name on stderr.

With CI credentials in the environment (`ASTER_SRV_TOKEN`, GitHub OIDC) the CI
token is used and reads its own repository. Not signed in, or a refused token,
exits non-zero with the reason.

## 4. Flaky and broken

The server classifies every run. A test is a **definite flake** when it failed
and then passed on a retry in one run, or when two runs with the same target
`input_hash` (the same inputs) disagree. Each failing run counts once. When
many of a target run's tests fail together (a crashed shard or a dead
database), the server records one **incident** for the target run instead of
a flake per test. A test is **broken** when its last 3 default-branch runs
failed the same way; broken tests are never quarantined.
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
from 2 different failing runs fall within 7 days, counting only flakes whose
runs are both default-branch runs by their credentials (a push, dispatch or
schedule of the default branch): a pull request job's results score flakes
but never quarantine. A run that flaked more than 10 tests counts toward none
of them, and an incident's failures never count. It releases the test after 20
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

## 6. Balanced partitions

A target that runs one partition of a Mix suite
(`MIX_TEST_PARTITION=K mix test --partitions N`) runs an explicit file list
when `aster-durations.json` at the workspace root times its suite: the
suite's files split longest-first by median duration. The file comes from
the checkout, so every shard of a commit splits the same way; nothing is
fetched while the suite runs. `aster results durations` writes it from the
server; commit it and refresh it on a schedule (see
[Durations](../crates/aster-srv/README.md#durations)). Without the file, the
suite in it, or with `ASTER_BALANCE_PARTITIONS=off`, every shard keeps Mix's
own split. A shard's tests are recorded under the project whose suite it
runs (the directory the command `cd`s to), so a test has one id, and one
quarantine entry, whichever shard ran it.

## 7. Notifications

The server can POST to webhooks when a test is quarantined, released, expires
from quarantine, or breaks. See
[Webhooks](../crates/aster-srv/README.md#webhooks).

## 8. Self-hosting

aster-srv is one binary (and the image
`ghcr.io/archastro/aster-srv:<aster version>`) that needs Postgres. The
[server README](../crates/aster-srv/README.md) covers configuration,
authentication, the dashboard and operating it.
