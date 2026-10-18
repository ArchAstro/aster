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
//! | `POST /v1/cache/commit` | Store a manifest once every blob it names exists. `409` if the key is already stored; `422` if a blob is missing. Writers only. |

mod policy;
mod routes;
mod signing;
mod store;

use crate::db::Migration;

pub use routes::{handle, Headers};
pub use signing::{expires_at, FixedSigner, Signer, URL_TTL};
pub use store::{run_retention, spawn, BlobStore, Report};

pub const MIGRATIONS: &[Migration] = &[Migration {
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
}];
