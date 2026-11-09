// Model of aster-srv's directory cache: what `upload`, `commit`, the retention
// pass and `dir-lookup` do to the bucket, the blob rows and the manifests
// (crates/aster-srv/src/cache/routes.rs and store.rs).
//
// A blob is a chunk's content address. The bucket can hold bytes that do not
// hash to their name (`corrupt`): a signed PUT is not checked when it happens.
// A key is a family plus a generation (11 is family 10, generation 1), so a
// restore key is a family.
//
// `commit` is two steps on purpose. `vouch` hashes new blobs outside the
// transaction and records each as it goes; `finish` runs under the lock that
// retention takes exclusively. `properties.qnt` lets retention and other
// commits run between them. `scenarios.qnt` drives the real server through the
// whole `commit`.
//
// `Rules` holds the two decisions a retried commit depends on, so the bug each
// one prevents can be shown in `witnesses.qnt`:
//   rereadRows       finish reads the blob rows again under the lock
//   keepVouchedRows  retention leaves a recent row no manifest names yet
module dircache {
  type Key = int
  type Blob = int
  type Hit = { found: bool, key: Key, blobs: Set[Blob] }
  type Rules = { rereadRows: bool, keepVouchedRows: bool }

  type State = {
    objects: Set[Blob],
    corrupt: Set[Blob],
    rows: Set[Blob],
    entries: Key -> Set[Blob],
    // A key's position among the stored keys: 0 was used last (`last_used_at`).
    recent: Key -> int,
    // A key's position among the stored keys: 0 was created last (`created_at`).
    newest: Key -> int,
    // The answer to the last commit: none, stored, exists, missing.
    reply: str,
    missing: Set[Blob],
    hit: Hit,
  }

  pure val mainRules: Rules = { rereadRows: true, keepVouchedRows: true }
  // The conformance server caps directory entries at 25 bytes and stores
  // 10-byte blobs, so two blobs fit and three do not.
  pure val capBlobs: int = 2
  // One lookup looks at this many entries (MAX_BROKEN_DIR_ENTRIES + 1).
  pure val lookupTries: Set[int] = Set(1, 2, 3, 4)
  pure val noHit: Hit = { found: false, key: 0, blobs: Set() }

  pure val empty: State = {
    objects: Set(),
    corrupt: Set(),
    rows: Set(),
    entries: Map(),
    recent: Map(),
    newest: Map(),
    reply: "none",
    missing: Set(),
    hit: noHit,
  }

  pure def inFamily(key: Key, family: int): bool = key >= family and key < family + 10

  pure def has(s: State, key: Key): bool = s.entries.keys().contains(key)

  // The key at a position, or 0 past the end. One key at most can match, so
  // the order of this fold does not matter.
  pure def keyAt(order: Key -> int, pos: int): Key =
    order.keys().filter(k => order.get(k) == pos).fold(0, (_, k) => k)

  pure def renumber(order: Key -> int, kept: Set[Key]): Key -> int =
    kept.mapBy(k => kept.filter(o => order.get(o) < order.get(k)).size())

  pure def moveToFront(order: Key -> int, key: Key): Key -> int =
    val at = if (order.keys().contains(key)) order.get(key) else order.keys().size()
    order.keys().union(Set(key)).mapBy(k =>
      if (k == key) 0
      else if (order.get(k) < at) order.get(k) + 1
      else order.get(k))

  pure def dropEntry(s: State, key: Key): State =
    val kept = s.entries.keys().exclude(Set(key))
    s.with("entries", kept.mapBy(k => s.entries.get(k)))
     .with("recent", renumber(s.recent, kept))
     .with("newest", renumber(s.newest, kept))

  pure def upload(s: State, blob: Blob): State =
    s.with("objects", s.objects.union(Set(blob)))
     .with("corrupt", s.corrupt.exclude(Set(blob)))

  pure def uploadForged(s: State, blob: Blob): State =
    s.with("objects", s.objects.union(Set(blob)))
     .with("corrupt", s.corrupt.union(Set(blob)))

  pure def loseObject(s: State, blob: Blob): State =
    s.with("objects", s.objects.exclude(Set(blob)))
     .with("corrupt", s.corrupt.exclude(Set(blob)))

  // `POST /upload`: a row whose object is gone is dropped, so that a later
  // upload can replace it. An entry that names the blob keeps naming it.
  pure def askToUpload(s: State, blobs: Set[Blob]): State =
    s.with("rows", s.rows.exclude(blobs.filter(b => not(s.objects.contains(b)))))

  // Hash every blob with no row. Good ones get a row now, whatever happens to
  // this commit. A forged object is deleted so the client can upload again.
  pure def vouch(s: State, blobs: Set[Blob]): State =
    val unrecorded = blobs.filter(b => not(s.rows.contains(b)))
    val good = unrecorded.filter(b => s.objects.contains(b) and not(s.corrupt.contains(b)))
    val forged = unrecorded.filter(b => s.objects.contains(b) and s.corrupt.contains(b))
    val bad = unrecorded.exclude(good)
    s.with("rows", s.rows.union(good).exclude(bad))
     .with("objects", s.objects.exclude(forged))
     .with("corrupt", s.corrupt.exclude(forged))
     .with("missing", bad)
     .with("reply", if (bad == Set()) s.reply else "missing")

  // Under the lock. A blob with a row needs only a HEAD; one without is
  // hashed. Anything wrong rolls the manifest back and forgets the rows.
  pure def finish(s: State, rules: Rules, key: Key, blobs: Set[Blob]): State =
    val recorded = if (rules.rereadRows) s.rows else blobs
    val present = blobs.filter(b => recorded.contains(b) and s.objects.contains(b))
    val hashed = blobs.exclude(present)
      .filter(b => s.objects.contains(b) and not(s.corrupt.contains(b)))
    val bad = blobs.exclude(present).exclude(hashed)
    val forged = bad.filter(b => s.objects.contains(b))
    if (has(s, key))
      s.with("reply", "exists").with("missing", Set())
    else if (bad != Set())
      s.with("rows", s.rows.exclude(bad))
       .with("objects", s.objects.exclude(forged))
       .with("corrupt", s.corrupt.exclude(forged))
       .with("reply", "missing")
       .with("missing", bad)
    else
      s.with("rows", s.rows.union(blobs))
       .with("entries", s.entries.put(key, blobs))
       .with("recent", moveToFront(s.recent, key))
       .with("newest", moveToFront(s.newest, key))
       .with("reply", "stored")
       .with("missing", Set())

  pure def commit(s: State, rules: Rules, key: Key, blobs: Set[Blob]): State =
    if (has(s, key))
      s.with("reply", "exists").with("missing", Set())
    else
      val vouched = vouch(s, blobs)
      if (vouched.missing != Set()) vouched
      else finish(vouched, rules, key, blobs)

  type Walk = { at: int, seen: Set[Blob], sized: Set[Blob], kept: Set[Key], released: Set[Blob] }

  // From the most recent use back. An entry stays if it fits with what stayed
  // before it, and the first entry always stays. A blob with no row counts for
  // nothing. The fold only counts steps; `walk.at` picks each key.
  pure def evict(s: State, expired: Set[Key]): Walk =
    s.entries.keys().fold({ at: 0, seen: Set(), sized: Set(), kept: Set(), released: Set() }, (walk, _) =>
      val key = keyAt(s.recent, walk.at)
      val moved = walk.with("at", walk.at + 1)
      val blobs = s.entries.get(key)
      val sized = walk.sized.union(blobs.filter(b => s.rows.contains(b)))
      if (expired.contains(key))
        moved
      else if (walk.seen != Set() and sized.size() > capBlobs)
        moved.with("released", walk.released.union(blobs))
      else
        moved.with("seen", walk.seen.union(blobs)).with("sized", sized).with("kept", walk.kept.union(Set(key)))
    )

  pure def blobsOf(s: State, ks: Set[Key]): Set[Blob] =
    ks.fold(Set(), (acc, key) => acc.union(s.entries.get(key)))

  // Drop `expired`, evict past the cap, and delete the blobs nothing names.
  pure def retain(s: State, rules: Rules, expired: Set[Key]): State =
    val walk = evict(s, expired)
    val keep = blobsOf(s, walk.kept)
    val released = walk.released.union(blobsOf(s, expired.filter(k => has(s, k))))
    val orphans = s.rows.filter(b =>
      not(keep.contains(b)) and (released.contains(b) or not(rules.keepVouchedRows)))
    s.with("entries", walk.kept.mapBy(k => s.entries.get(k)))
     .with("recent", renumber(s.recent, walk.kept))
     .with("newest", renumber(s.newest, walk.kept))
     .with("rows", s.rows.exclude(orphans))
     .with("objects", s.objects.exclude(orphans))
     .with("corrupt", s.corrupt.exclude(orphans))

  pure def newestIn(s: State, family: int): Key =
    val members = s.entries.keys().filter(k => inFamily(k, family))
    members.filter(k => members.forall(o => s.newest.get(k) <= s.newest.get(o))).fold(0, (_, k) => k)

  // The key to serve: `key` itself, else the newest entry of the first family
  // that has one. 0 when there is none; real keys start at 10.
  pure def resolve(s: State, key: Key, families: List[int]): Key =
    val matching = families.indices().filter(i => newestIn(s, families[i]) != 0)
    val first = matching.filter(i => matching.forall(j => i <= j))
    if (has(s, key)) key else first.fold(0, (_, i) => newestIn(s, families[i]))

  type Search = { s: State, done: bool }

  // Serve the first entry whose blobs are all in the bucket. An entry that has
  // lost one can never be restored, so it is dropped on the way past. The fold
  // only counts tries.
  pure def lookup(s: State, key: Key, families: List[int]): State =
    val walked = lookupTries.fold({ s: s.with("hit", noHit), done: false }, (search, _) =>
      val at = resolve(search.s, key, families)
      if (search.done or at == 0)
        search.with("done", true)
      else if (search.s.entries.get(at).exclude(search.s.objects) == Set())
        { s: search.s
            .with("hit", { found: true, key: at, blobs: search.s.entries.get(at) })
            .with("recent", moveToFront(search.s.recent, at)),
          done: true }
      else
        search.with("s", dropEntry(search.s, at))
    )
    walked.s
}
