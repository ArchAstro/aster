# Directory cache model

A [Quint](https://quint-lang.org) model of what aster-srv does for
`aster cache save` and `restore`, checked against the real server with
[quint-refinements](https://github.com/ArchAstro/quint-refinements).

## What the model says

| File | Content |
|---|---|
| `model.qnt` | Bucket objects, blob rows, manifests, both orderings. `upload`, `askToUpload`, `commit` (`vouch` then `finish`), `retain`, `lookup`. |
| `scenarios.qnt` | 18 histories. Each `@conformance` run also runs against the server. |
| `properties.qnt` | Invariants while commits, retention and lookups interleave. |
| `witnesses.qnt` | The failure each rule in `Rules` prevents, shown by turning it off. |

`live.rs` sends each scenario step to a server on a fresh Postgres database.
After each step the checker compares the bucket, blob rows, manifests, orderings
and last replies with the state Quint computed.

## Properties

`quint:properties` runs random histories with retention allowed between a
commit's two halves. This is simulation (2,000 traces of 14 steps), not an
exhaustive proof, and it treats `finish` and a retention pass as atomic: on the
server the advisory lock makes them so.

1. A stored entry's chunks are in the bucket and have rows.
2. No row or entry names bytes that do not hash to their name.
3. A served hit is a stored entry whose chunks are in the bucket.
4. The orderings list exactly the stored keys, with no gaps.
5. After a retention pass, entries fit the cap or only one remains.

A second run adds objects vanishing from the bucket and the upload route. Then
1 and the entry half of 2 do not hold, so it checks the rest
(`holdDespiteLoss`). `bytesForgedAfterALossAreServedAndLeftToTheClientRun` shows
why: the server serves a chunk replaced after a loss without hashing it. The
client hashes every chunk (`get_blob`) and treats a mismatch as a miss.

## Running

Needs Node 22, Rust, and a Postgres whose user may create databases.

```bash
npm ci
npm run check             # generated files match the model
npm run typecheck
npm run quint:test        # witnesses
npm run quint:properties
ASTER_SRV_TEST_DATABASE_URL=postgres://... cargo test --locked
```

Without `ASTER_SRV_TEST_DATABASE_URL` the conformance test fails. It does not skip.

## Not modelled

1. **Interleavings on the server.** Refinement runs one step at a time. The race
   between `finish` and retention is only simulated in the model.
   `crates/aster-srv/tests/cache.rs` holds the concurrent tests.
2. **The client.** Tar, chunking, extraction, digest checks and restore atomicity
   are covered by `src/cache/dir_cache.rs` tests and `tests/cache_remote_e2e.rs`.
3. **Real GCS.** The bucket is `BlobStore::memory`, and `lose` goes through
   `BlobStore::delete`, which also clears the presence cache. A real loss can be
   served from that cache for up to `present_ttl` (1h); the download then fails
   and the client reports a miss.
4. **Signed PUT lifetime.** A forged upload is only modelled for a blob with no
   row. A URL issued earlier (valid 15 minutes) could overwrite a recorded
   object. Only main-push writers get URLs, and the client's digest check applies.
5. **Who may read or write,** and lookups that do not bump `last_used_at`.
6. **Age and scope of retention.** Row expiry by age, deletion of objects with no
   row, the per-repo cap (scenarios use one repo), and `keep` being computed over
   all manifests, including target-cache ones.
7. **Limits and timing.** The lookup deadline (`tests/cache.rs`),
   `MAX_LOOKUP_BLOBS`, and blob-row `last_used_at` bumps on a hit.
