// Histories run against the real aster-srv: its routes, its migrations, a real
// Postgres, and the in-memory bucket. Each one checks every step's bucket, blob
// rows, manifests, orderings and replies against the model.
module scenarios {
  import dircache.* from "./model"

  var state: State

  pure val noFamilies: List[int] = []
  pure val oneFamily: List[int] = [10]
  pure val bothFamilies: List[int] = [30, 10]

  action init = state' = empty
  action putObject(blob: Blob): bool = state' = upload(state, blob)
  action putForged(blob: Blob): bool = state' = uploadForged(state, blob)
  action lose(blob: Blob): bool = state' = loseObject(state, blob)
  action requestUpload(blobs: Set[Blob]): bool = state' = askToUpload(state, blobs)
  action commitEntry(key: Key, blobs: Set[Blob]): bool = state' = commit(state, mainRules, key, blobs)
  action runRetention(expired: Set[Key]): bool = state' = retain(state, mainRules, expired)
  action dirLookup(key: Key, families: List[int]): bool = state' = lookup(state, key, families)

  /// @conformance
  run storedEntryIsAnExactHitRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1, 2)))
    .then(dirLookup(11, noFamilies))
    .then(all { assert(state.hit == { found: true, key: 11, blobs: Set(1, 2) }), state' = state })

  /// @conformance
  run unknownKeyIsAMissRun = init
    .then(dirLookup(11, oneFamily))
    .then(all { assert(not(state.hit.found)), state' = state })

  /// @conformance
  run storingAKeyTwiceAnswersExistsRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1)))
    .then(commitEntry(11, Set(2)))
    .then(all { assert(state.reply == "exists" and state.entries.get(11) == Set(1)), state' = state })

  /// @conformance
  run aMissingChunkIsReportedAndTheOthersKeepTheirRowsRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1, 2)))
    .then(all {
      assert(state.reply == "missing" and state.missing == Set(2)),
      assert(state.rows == Set(1) and state.entries.keys() == Set()),
      state' = state,
    })

  /// @conformance
  run aRetryResumesAcrossARetentionPassRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1, 2)))
    .then(runRetention(Set()))
    .then(putObject(2))
    .then(commitEntry(11, Set(1, 2)))
    .then(all { assert(state.reply == "stored" and state.objects == Set(1, 2)), state' = state })

  /// @conformance
  run forgedBytesAreDeletedAndCanBeUploadedAgainRun = init
    .then(putForged(1))
    .then(commitEntry(11, Set(1)))
    .then(all {
      assert(state.reply == "missing" and state.objects == Set() and state.rows == Set()),
      state' = state,
    })
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(all { assert(state.reply == "stored"), state' = state })

  /// @conformance
  run aRecordedChunkGoneFromTheBucketFailsTheCommitRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(lose(1))
    .then(commitEntry(12, Set(1)))
    .then(all {
      assert(state.reply == "missing" and state.rows == Set()),
      assert(state.entries.keys() == Set(11)),
      state' = state,
    })

  /// @conformance
  run exactBeatsNewestAndNewestWinsAPrefixRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1)))
    .then(commitEntry(12, Set(2)))
    .then(dirLookup(11, oneFamily))
    .then(all { assert(state.hit.key == 11), state' = state })
    .then(dirLookup(19, oneFamily))
    .then(all { assert(state.hit.key == 12), state' = state })

  /// @conformance
  run theFirstFamilyWithAnEntryAnswersRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1)))
    .then(commitEntry(21, Set(2)))
    .then(dirLookup(39, bothFamilies))
    .then(all { assert(state.hit.key == 11), state' = state })

  /// @conformance
  run anEntryThatLostAChunkIsDroppedAndTheNextNewestServesRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1)))
    .then(commitEntry(12, Set(2)))
    .then(lose(2))
    .then(dirLookup(12, oneFamily))
    .then(all { assert(state.hit.key == 11 and state.entries.keys() == Set(11)), state' = state })

  /// @conformance
  run aKeyThatLostAChunkCanBeStoredAgainRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(lose(1))
    .then(dirLookup(11, noFamilies))
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(all { assert(state.reply == "stored"), state' = state })

  /// @conformance
  run oneLookupDropsAtMostFourBrokenEntriesRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(commitEntry(12, Set(1)))
    .then(commitEntry(13, Set(1)))
    .then(commitEntry(14, Set(1)))
    .then(commitEntry(15, Set(1)))
    .then(lose(1))
    .then(dirLookup(19, oneFamily))
    .then(all { assert(not(state.hit.found) and state.entries.keys() == Set(11)), state' = state })

  /// @conformance
  run retentionDropsExpiredEntriesAndKeepsSharedChunksRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1, 2)))
    .then(commitEntry(21, Set(2)))
    .then(runRetention(Set(11)))
    .then(all {
      assert(state.entries.keys() == Set(21)),
      assert(state.objects == Set(2) and state.rows == Set(2)),
      state' = state,
    })

  /// @conformance
  run retentionEvictsTheLeastRecentlyUsedOverTheCapRun = init
    .then(putObject(1)).then(putObject(2)).then(putObject(3))
    .then(commitEntry(11, Set(1)))
    .then(commitEntry(12, Set(2)))
    .then(commitEntry(13, Set(3)))
    .then(dirLookup(11, noFamilies))
    .then(runRetention(Set()))
    .then(all {
      assert(state.entries.keys() == Set(11, 13)),
      assert(state.objects == Set(1, 3)),
      state' = state,
    })

  /// @conformance
  run aSharedChunkCountsOnceAgainstTheCapRun = init
    .then(putObject(1)).then(putObject(2))
    .then(commitEntry(11, Set(1, 2)))
    .then(commitEntry(12, Set(2)))
    .then(runRetention(Set()))
    .then(all { assert(state.entries.keys() == Set(11, 12)), state' = state })

  /// @conformance
  run askingToUploadALostChunkDropsItsRowRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(lose(1))
    .then(requestUpload(Set(1)))
    .then(all { assert(state.rows == Set() and state.entries.keys() == Set(11)), state' = state })

  // The server does not hash a chunk on lookup, so it serves these bytes. The
  // client checks every chunk's digest and treats this as a miss.
  /// @conformance
  run bytesForgedAfterALossAreServedAndLeftToTheClientRun = init
    .then(putObject(1))
    .then(commitEntry(11, Set(1)))
    .then(lose(1))
    .then(requestUpload(Set(1)))
    .then(putForged(1))
    .then(dirLookup(11, noFamilies))
    .then(all { assert(state.hit.found and state.corrupt == Set(1)), state' = state })

  /// @conformance
  run theMostRecentEntryStaysEvenAloneRun = init
    .then(putObject(1)).then(putObject(2)).then(putObject(3))
    .then(commitEntry(11, Set(1, 2, 3)))
    .then(runRetention(Set()))
    .then(all { assert(state.entries.keys() == Set(11)), state' = state })
}
