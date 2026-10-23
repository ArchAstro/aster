//! The shared artifact cache: manifests in Postgres, blobs in GCS.
//!
//! Clients never hold bucket credentials. They ask this server for short-lived
//! V4 signed URLs and move blob bytes directly to and from GCS. Manifests stay
//! rows so the server can authorise and expire them.
//!
//! | Route | |
//! |---|---|
//! | `POST /v1/cache/lookup` | Hits for the named keys, each with a signed GET per blob that exists. An unknown key is a miss, not an error. |
//! | `POST /v1/cache/upload` | Signed PUT URLs for blobs not yet in the bucket. Writers only. |
//! | `POST /v1/cache/commit` | Store a manifest once every blob it names exists, and record who wrote it. `409` if the key is already stored; `422` if a blob is missing. Writers only. |
//! | `GET /v1/cache/summary` | Counts, sizes, last writer and 14 days of lookups for one repository. Read-only. |
//! | `GET /v1/cache/entries` | A page of manifests with their writer and hit counts. Read-only. |

pub(crate) mod browse;
mod policy;
mod routes;
mod signing;
mod store;

use crate::db::Migration;

pub use policy::{cache_writer_allowed, results_writer_allowed, workflow_allowed};

/// Whether a signed-in person may read `repo`'s data: their email domain is
/// allowed and the server serves the repository.
pub fn person_may_browse(
    app: &crate::App,
    person: &crate::person::Person,
    repo: &str,
) -> Result<(), &'static str> {
    policy::person_browser(&app.iap, person, repo, &app.config.repositories)
}
pub use routes::{handle, Headers};
pub use signing::{expires_at, FixedSigner, Signer, URL_TTL};
pub use store::{run_retention, spawn, BlobStore, Report};

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        feature: "cache",
        version: 1,
        sql: r#"
CREATE SCHEMA IF NOT EXISTS cache;

CREATE TABLE cache.manifests (
    repo text NOT NULL,
    key text NOT NULL,
    manifest bytea NOT NULL,
    size_bytes bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repo, key)
);

CREATE TABLE cache.blobs (
    sha256 text PRIMARY KEY,
    size_bytes bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now()
);
"#,
    },
    Migration {
        feature: "cache",
        version: 2,
        sql: r#"
ALTER TABLE cache.manifests
    ADD COLUMN writer_ref text NULL,
    ADD COLUMN writer_run_id text NULL,
    ADD COLUMN writer_sha text NULL,
    ADD COLUMN writer_workflow text NULL,
    ADD COLUMN hits bigint NOT NULL DEFAULT 0,
    ADD COLUMN last_hit_at timestamptz NULL;

CREATE TABLE cache.lookup_stats (
    repo text NOT NULL,
    day date NOT NULL,
    hits bigint NOT NULL DEFAULT 0,
    misses bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (repo, day)
);
"#,
    },
    Migration {
        feature: "cache",
        version: 3,
        sql: r#"
ALTER TABLE cache.manifests
    ADD COLUMN target text NOT NULL DEFAULT '',
    ADD COLUMN output_files int NOT NULL DEFAULT 0,
    ADD COLUMN exit_code int NOT NULL DEFAULT 0,
    ADD COLUMN duration_ms bigint NOT NULL DEFAULT 0,
    ADD COLUMN aster_version text NOT NULL DEFAULT '';

-- A manifest that is not UTF-8 JSON gives NULL, so its row keeps the defaults.
CREATE FUNCTION cache.manifest_json(bytes bytea) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    RETURN convert_from(bytes, 'UTF8')::jsonb;
EXCEPTION WHEN OTHERS THEN
    RETURN NULL;
END
$$;

-- Each field is taken only when it has the type the browse page reads, and
-- the number fits its column, as when a commit fills it.
UPDATE cache.manifests m SET
    target = CASE WHEN jsonb_typeof(j->'target') = 'string' THEN j->>'target' ELSE '' END,
    output_files = CASE WHEN jsonb_typeof(j->'outputs') = 'array'
        THEN least(jsonb_array_length(j->'outputs'), 2147483647) ELSE 0 END,
    -- Postgres does not order the operands of AND, so the casts sit in nested
    -- CASEs: a value is cast to numeric only once it is known to be a number,
    -- and to the column type only once it is known to fit.
    exit_code = CASE WHEN jsonb_typeof(j->'exit_code') = 'number' THEN
            CASE WHEN (j->>'exit_code')::numeric = trunc((j->>'exit_code')::numeric)
                    AND (j->>'exit_code')::numeric BETWEEN -2147483648 AND 2147483647
                THEN (j->>'exit_code')::numeric::int ELSE 0 END
        ELSE 0 END,
    duration_ms = CASE WHEN jsonb_typeof(j->'duration_ms') = 'number' THEN
            CASE WHEN (j->>'duration_ms')::numeric = trunc((j->>'duration_ms')::numeric)
                    AND (j->>'duration_ms')::numeric BETWEEN 0 AND 9223372036854775807
                THEN (j->>'duration_ms')::numeric::bigint ELSE 0 END
        ELSE 0 END,
    aster_version = CASE WHEN jsonb_typeof(j->'aster_version') = 'string'
        THEN j->>'aster_version' ELSE '' END
FROM (SELECT repo, key, cache.manifest_json(manifest) AS j FROM cache.manifests) parsed
WHERE parsed.repo = m.repo AND parsed.key = m.key
    AND jsonb_typeof(parsed.j) = 'object';

DROP FUNCTION cache.manifest_json(bytea);

CREATE INDEX manifests_browse ON cache.manifests (repo, created_at DESC, key);
"#,
    },
];
