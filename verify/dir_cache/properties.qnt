// Invariants of the directory cache while commits, retention and lookups
// interleave. `beginCommit` and `finishCommit` are the two halves of a commit,
// so retention can run between them, as it can on the server:
//
//   quint run properties.qnt --main properties --init start --step step --invariant allHold
module properties {
  import dircache.* from "./model"

  pure val blobs: Set[Blob] = Set(1, 2, 3, 4)
  pure val allKeys: Set[Key] = Set(11, 12, 21)
  pure val chunkSets: Set[Set[Blob]] = Set(Set(1), Set(2), Set(1, 2), Set(2, 3), Set(3, 4))

  type Commit = { key: Key, blobs: Set[Blob] }

  var state: State
  var inflight: Set[Commit]
  // The step just taken: retention, lookup, or other.
  var last: str

  action start = all {
    state' = empty,
    inflight' = Set(),
    last' = "other",
  }

  action settle(after: State): bool = all {
    state' = after,
    inflight' = inflight,
    last' = "other",
  }

  action step = any {
    nondet b = oneOf(blobs)
    settle(upload(state, b)),
    // A signed PUT URL is only handed out for a blob with no row.
    nondet b = oneOf(blobs)
    all { not(state.rows.contains(b)), settle(uploadForged(state, b)) },
    nondet k = oneOf(allKeys)
    nondet bs = oneOf(chunkSets)
    settle(commit(state, mainRules, k, bs)),
    nondet k = oneOf(allKeys)
    nondet bs = oneOf(chunkSets)
    val vouched = vouch(state, bs)
    all {
      vouched.missing == Set(),
      state' = vouched,
      inflight' = inflight.union(Set({ key: k, blobs: bs })),
      last' = "other",
    },
    nondet c = oneOf(inflight)
    all {
      state' = finish(state, mainRules, c.key, c.blobs),
      inflight' = inflight.exclude(Set(c)),
      last' = "other",
    },
    nondet expired = oneOf(allKeys.powerset())
    all {
      state' = retain(state, mainRules, expired),
      inflight' = inflight,
      last' = "retention",
    },
    nondet k = oneOf(allKeys)
    nondet family = oneOf(Set(10, 20))
    all {
      state' = lookup(state, k, [family]),
      inflight' = inflight,
      last' = "lookup",
    },
  }

  // `step`, plus objects vanishing from the bucket and the upload route that
  // repairs the rows afterwards.
  action stepWithLoss = any {
    step,
    nondet b = oneOf(blobs)
    settle(loseObject(state, b)),
    nondet bs = oneOf(chunkSets)
    settle(askToUpload(state, bs)),
  }

  val named: Set[Blob] = blobsOf(state, state.entries.keys())

  // A stored entry's chunks are all in the bucket, with a row.
  val namedBlobsExist: bool = named.subseteq(state.objects) and named.subseteq(state.rows)
  // Nothing vouches for bytes that do not hash to their name.
  val rowsAreVerified: bool = state.rows.intersect(state.corrupt) == Set()
  val entriesAreVerified: bool = named.intersect(state.corrupt) == Set()
  // A served hit is a stored entry with its chunks in the bucket.
  val hitsArePresent: bool =
    (last == "lookup" and state.hit.found) implies
      (state.hit.blobs.subseteq(state.objects) and has(state, state.hit.key))
  // The orderings list exactly the stored keys.
  val orderingsAgree: bool = and {
    state.recent.keys() == state.entries.keys(),
    state.newest.keys() == state.entries.keys(),
    state.entries.keys().map(k => state.recent.get(k)) == 0.to(state.entries.keys().size() - 1),
    state.entries.keys().map(k => state.newest.get(k)) == 0.to(state.entries.keys().size() - 1),
  }
  // After a pass the entries fit, or one entry alone is too big.
  val capHolds: bool =
    (last == "retention") implies (named.intersect(state.rows).size() <= capBlobs or state.entries.keys().size() <= 1)

  // With loss, an entry can name a chunk that is gone or was forged after the
  // loss. Rows stay honest, and a served hit still has its chunks present.
  val holdDespiteLoss: bool = and {
    rowsAreVerified, hitsArePresent, orderingsAgree, capHolds,
  }

  val allHold: bool = and {
    namedBlobsExist, rowsAreVerified, entriesAreVerified, hitsArePresent, orderingsAgree, capHolds,
  }
}
