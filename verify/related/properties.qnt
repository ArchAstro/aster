// Properties of the selection model, checked on random repos built from
// the shop's definitions with random references between them:
//
//   quint run properties.qnt --main properties --init start --step step --invariant allHold
//
// and the divergences the model finds, which are demonstrated on fixed repos
// in `witnesses` (run with `quint test`).
module properties {
  import related.* from "./model"
  import repos.* from "./repos"

  // References a random repo may contain. A pair names a definition id.
  pure val callers: Set[int] = Set(20, 21, 22, 63, 72, 94, 1010, 1020, 1060, 1080, 1090, 1100)
  pure val callees: Set[int] = Set(10, 11, 12, 20, 21, 30, 31, 32, 40, 50, 52, 60, 62, 72, 80, 81, 90, 91)
  pure val candidateCalls: Set[Pair] =
    callers.map(a => callees.filter(b => a != b and unit(shop, a) != unit(shop, b)).map(b => { from: a, to: b })).flatten()
      .filter(p => p.from % 3 == p.to % 3 or p.from % 5 == 0)
  pure val candidateShapes: Set[Pair] = Set(
    { from: 22, to: 5 }, { from: 51, to: 6 }, { from: 1080, to: 5 }, { from: 1090, to: 6 }, { from: 20, to: 6 })
  pure val candidateImports: Set[Pair] = Set({ from: 104, to: 4 }, { from: 101, to: 1 }, { from: 2, to: 1 }, { from: 106, to: 2 })
  pure val candidateBare: Set[Pair] = Set(
    { from: 63, to: 62 }, { from: 1040, to: 40 }, { from: 1010, to: 10 }, { from: 20, to: 10 }, { from: 21, to: 20 })
  pure val candidateRequests: Set[Request] = Set(
    { id: 1030, path: List(1, 0) }, { id: 1090, path: List(2, 0) }, { id: 1010, path: List(1) },
    { id: 22, path: List(2, 5) }, { id: 1060, path: List(1, 0, 3) })
  pure val editable: Set[int] = shop.kindOf.keys()
  pure val allRules: Set[Rules] = Set(
    mainRules,
    mainRules.with("schemaTop", true),
    mainRules.with("routeNothing", true),
    mainRules.with("schemaTop", true).with("routeNothing", true),
    mainRules.with("fullFallback", true),
    mainRules.with("sendSites", false),
    mainRules.with("callbackClauses", false).with("deadSilent", false).with("processStarters", false))

  var repo: Repo
  var rules: Rules
  var small: Changes
  var big: Changes

  action start = {
    nondet calls = oneOf(candidateCalls.powerset())
    nondet shapes = oneOf(candidateShapes.powerset())
    nondet imports = oneOf(candidateImports.powerset())
    nondet bare = oneOf(candidateBare.powerset())
    nondet requests = oneOf(candidateRequests.powerset())
    nondet reflects = oneOf(Set(Set(), Set(1050), Set(1050, 21)))
    nondet preloads = oneOf(Set(Set(), Set({ from: 1070, to: 6 }), Set({ from: 1030, to: 8 })))
    nondet builtPaths = oneOf(Set(Set(), Set({ from: 1070, to: 70 })))
    nondet computed = oneOf(Set(Set(), Set({ from: 1070, to: 91 }), Set({ from: 1020, to: 10 })))
    nondet chosen = oneOf(allRules)
    nondet picked = oneOf(editable.powerset())
    nondet fewer = oneOf(picked.powerset())
    val r0 = shop
      .with("calls", calls).with("shapes", shapes).with("imports", imports)
      .with("requests", requests).with("reflects", reflects).with("preloads", preloads)
      .with("builtPaths", builtPaths).with("computed", computed)
    val validBare = bare.filter(p => unit(r0, p.from) == unit(r0, p.to) or importsUnit(r0, p.from, unit(r0, p.to)))
    all {
      repo' = r0.with("bare", validBare),
      rules' = chosen,
      big' = { edits: picked, comments: Set(), trailing: Set() },
      small' = { edits: fewer, comments: Set(), trailing: Set() },
    }
  }

  action step = all {
    repo' = repo, rules' = rules, small' = small, big' = big,
  }

  val sel = selectTests(repo, rules, big)

  // Soundness against the run-time oracle, outside the named carve-outs.
  val soundness = sound(repo, sel, big)
  // Every selected test has a witness path of modelled edges.
  val precision = witnessed(repo, rules, big) and linksJustified(repo, rules, big)
  // A superset of edits selects a superset of tests (without the full-run fallback #354 removed).
  val monotonic = (if (rules.fullFallback) monotonePrecise(repo, rules, small, big) else monotone(repo, rules, small, big))
  // Edits to comments alone select nothing.
  val commentsOnly =
    selectTests(repo, rules, { edits: Set(), comments: repo.unitOf.keys().map(d => unit(repo, d)), trailing: Set() }) == noSelection
  // Units flagged by the oracle as missed are only missed through a carve-out.
  val missedOnlyByCarveOut = sel.full or missedByCarveOut(repo, sel, big).forall(t =>
    runs(repo, allKinds, t).exists(d => big.edits.contains(d)))

  val holdsSoundness = wellFormed(repo) implies soundness
  val holdsPrecision = wellFormed(repo) implies precision
  val holdsMonotonic = wellFormed(repo) implies monotonic
  val holdsComments = wellFormed(repo) implies commentsOnly
  val holdsCarveOuts = wellFormed(repo) implies missedOnlyByCarveOut
  val allHold = holdsSoundness and holdsPrecision and holdsMonotonic and holdsComments and holdsCarveOuts
}
