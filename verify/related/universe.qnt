// Generated repos: the shop's definitions with a pseudo-random subset of the
// references a repo may hold between them. `genRepo(seed)` and `genChange(seed)`
// are deterministic, so the scenarios built from them are stable.
module universe {
  import related.* from "./model"
  import repos.* from "./repos"

  // References a generated repo may contain.
  pure val callers: Set[int] = Set(20, 21, 22, 63, 72, 94, 1010, 1020, 1060, 1080, 1090, 1100)
  pure val callees: Set[int] = Set(10, 11, 12, 20, 21, 30, 31, 32, 40, 50, 52, 60, 62, 72, 80, 81, 90, 91)
  pure val candidateCalls: Set[Pair] =
    callers.map(a => callees.filter(b => a != b and unit(shop, a) != unit(shop, b)).map(b => { from: a, to: b })).flatten()
  pure val candidateShapes: Set[Pair] = Set(
    { from: 22, to: 5 }, { from: 51, to: 6 }, { from: 1080, to: 5 }, { from: 1090, to: 6 }, { from: 20, to: 6 },
    { from: 1060, to: 5 }, { from: 52, to: 6 })
  pure val candidateImports: Set[Pair] = Set(
    { from: 104, to: 4 }, { from: 101, to: 1 }, { from: 2, to: 1 }, { from: 106, to: 2 }, { from: 9, to: 4 }, { from: 5, to: 6 })
  pure val candidateBare: Set[Pair] = Set(
    { from: 63, to: 62 }, { from: 1040, to: 40 }, { from: 1010, to: 10 }, { from: 20, to: 10 }, { from: 21, to: 20 },
    { from: 60, to: 62 }, { from: 91, to: 90 }, { from: 52, to: 50 })
  pure val candidateRequests: Set[Request] = Set(
    { id: 1030, path: List(1, 0) }, { id: 1090, path: List(2, 0) }, { id: 1010, path: List(1) },
    { id: 22, path: List(2, 5) }, { id: 1060, path: List(1, 0, 3) }, { id: 72, path: List(1, 7) })
  pure val candidatePreloads: Set[Pair] = Set({ from: 1070, to: 6 }, { from: 1030, to: 8 }, { from: 1010, to: 3 })
  pure val candidateComputed: Set[Pair] = Set({ from: 1070, to: 91 }, { from: 1020, to: 10 }, { from: 1090, to: 12 })
  pure val reflectors: Set[int] = Set(1050, 21, 1060)
  pure val editable: Set[int] = shop.kindOf.keys()

  // A number in 0..mod-1 for (seed, k).
  pure def mix(seed: int, k: int, mod: int): int = {
    val h = (seed * 7919 + k * 104729 + 13) * (seed + 3) * (k + 5)
    ((h / 31 + h / 1009 + h / 65537) % 1000003) % mod
  }

  pure def pairKey(p: Pair): int = p.from * 31 + p.to
  pure def choosePairs(seed: int, salt: int, ps: Set[Pair], oneIn: int): Set[Pair] =
    ps.filter(p => mix(seed, pairKey(p) + salt, oneIn) == 0)

  pure def applyDelta(base: Repo, d: Delta): Repo =
    base.with("calls", d.calls).with("bare", d.bare).with("imports", d.imports)
      .with("shapes", d.shapes).with("requests", d.requests).with("reflects", d.reflects)
      .with("preloads", d.preloads).with("builtPaths", d.builtPaths).with("computed", d.computed)

  pure def genDelta(seed: int): Delta = {
    val imports = choosePairs(seed, 3, candidateImports, 3)
    {
      calls: choosePairs(seed, 1, candidateCalls, 16),
      shapes: choosePairs(seed, 2, candidateShapes, 3),
      imports: imports,
      requests: candidateRequests.filter(q => mix(seed, q.id + 4, 3) == 0),
      reflects: reflectors.filter(d => mix(seed, d + 5, 6) == 0),
      preloads: choosePairs(seed, 6, candidatePreloads, 2),
      builtPaths: if (mix(seed, 7, 2) == 0) Set({ from: 1070, to: 70 }) else Set(),
      computed: choosePairs(seed, 8, candidateComputed, 2),
      // A bare call goes to the same unit or an imported one.
      bare: choosePairs(seed, 9, candidateBare, 2).filter(p => unit(shop, p.from) == unit(shop, p.to) or imports.contains({ from: unit(shop, p.from), to: unit(shop, p.to) })),
    }
  }

  pure def genRepo(seed: int): Repo = applyDelta(shop, genDelta(seed))

  // One to three edits, and sometimes a comment.
  pure def genChange(seed: int): Changes = {
    val picked = editable.filter(d => mix(seed, d + 11, 26) == 0)
    val anchor = if (mix(seed, 12, 2) == 0) Set(10, 20, 31, 40, 52, 62, 80, 81, 90, 91, 1010, 1080)
                 .filter(d => mix(seed, d + 13, 12) == 0) else Set()
    val fallback = if (picked.union(anchor).size() == 0) Set(10 + 10 * mix(seed, 14, 9)).filter(d => editable.contains(d)) else Set()
    { edits: picked.union(anchor).union(fallback), comments: if (mix(seed, 15, 4) == 0) Set(1, 101) else Set(), trailing: Set() }
  }
}
