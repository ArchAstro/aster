// What goes wrong without each rule in `Rules`, on the smallest history that
// shows it. `quint test --main witnesses --match 'Witness$'`.
module witnesses {
  import dircache.* from "./model"

  var state: State

  action settle(after: State): bool = state' = after

  // A row retention removed between a commit's two halves, then a forged
  // upload of that chunk. Without reading the rows again, finish would trust
  // its first answer and store an entry naming the forged bytes.
  pure val shared: Set[Blob] = Set(1)

  pure val raced: State =
    uploadForged(
      retain(vouch(commit(upload(empty, 1), mainRules, 11, shared), shared), mainRules, Set(11)),
      1)

  run staleRowsStoreForgedBytesWitness = all {
    assert(finish(raced, mainRules.with("rereadRows", false), 21, shared).entries.get(21) == shared),
    assert(finish(raced, mainRules.with("rereadRows", false), 21, shared).corrupt == shared),
    assert(finish(raced, mainRules, 21, shared).reply == "missing"),
    assert(not(has(finish(raced, mainRules, 21, shared), 21))),
    state' = empty,
  }

  // A retry resumes from the rows the first attempt wrote. Without the grace,
  // a pass between the attempts deletes the rows and the uploaded chunks.
  pure val interrupted: State = commit(upload(empty, 1), mainRules, 11, Set(1, 2))

  run retentionWouldLoseVerifiedChunksWitness = all {
    assert(interrupted.rows == Set(1) and interrupted.reply == "missing"),
    assert(retain(interrupted, mainRules, Set()).objects == Set(1)),
    assert(retain(interrupted, mainRules.with("keepVouchedRows", false), Set()).objects == Set()),
    state' = empty,
  }
}
