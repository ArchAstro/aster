# aster-srv

Aster's server. It is one service for what Aster runs need beyond a single
machine. Its first feature is CI test results; others, such as a shared build
cache, live beside it under their own routes and Postgres schema.

## Test results

`aster --test-results` records every test a run executes. With
`ASTER_SRV_URL` set, Aster uploads the results after the run; `aster results
upload <file>` sends a file by hand.

| Route | |
|---|---|
| `POST /v1/test-results/runs` | Store one gzip NDJSON results stream. `201` stored, `200` already stored (same `run_id`), `401` bad token, `403` repository not allowed or results for another repository, `413` too large. |
| `GET /healthz` | The process is up. |
| `GET /readyz` | The database answers. |

Rows are kept for the retention period (30 days by default). Day partitions
past it are dropped every six hours.

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

## Configuration

| Variable | Default | |
|---|---|---|
| `ASTER_SRV_DATABASE_URL` | required | Postgres URL. Its `sslmode` decides TLS: `disable`, `prefer` (default) or `require`. As with libpq's `require`, the connection is encrypted but the server certificate is not verified, which suits Cloud SQL's private IP. |
| `ASTER_SRV_REPOSITORIES` | required | Comma-separated `owner/name` or `owner/*`. |
| `ASTER_SRV_LISTEN` | `0.0.0.0:8080` | |
| `ASTER_SRV_AUDIENCE` | `aster-srv` | The OIDC audience. |
| `ASTER_SRV_ISSUER` | GitHub Actions | |
| `ASTER_SRV_RETENTION_DAYS` | `30` | |
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
