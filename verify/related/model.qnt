// Model of the Elixir path through `aster affected <target> --related`
// (src/related/engine.rs and elixir.rs).
//
// A repo is a handful of definitions, each in a unit (one file and one module),
// plus the references between them. A change edits definitions and comments.
// `selectTests` is the selection function the engine is checked against:
// `scenarios.qnt` runs it side by side with the real engine.
//
// Definition kinds: "fn", "callback" (a name the runtime invokes), "macro",
// "route" (a router declaration), "top" (module-level code of a unit) and
// "test" (an ExUnit test).
//
// Rules that pull requests change are small switches in `Rules`, so each one can
// be compared with the rule on main:
//   callbackClauses  ArchAstro/aster#347 (merged: on in `mainRules`)
//   deadSilent       ArchAstro/aster#353 (merged: on)
//   processStarters  ArchAstro/aster#353 (merged: on)
//   fullFallback     the "reaches no test, run in full" check that
//                    ArchAstro/aster#354 removed (merged: off)
//   schemaTop        ArchAstro/aster#348 (open)
//   routeNothing     ArchAstro/aster#349 (open)
//   sendSites        naming a unit's module (a struct, a child specification) does not make a
//                    definition a sender of its messages; handing it to a call does (on)
module related {
  type Pair = { from: int, to: int }
  type Request = { id: int, path: List[int] }
  type Send = { id: int, tag: int }
  type Link = { from: int, to: int, rule: str }
  type Exec = { from: int, to: int, kind: str }

  type Repo = {
    // Per definition id.
    kindOf: int -> str,
    unitOf: int -> int,
    nameOf: int -> str,
    // Units that are test files.
    testUnits: Set[int],
    // `Shop.M<unit of to>.<name of to>()` written in `from`.
    calls: Set[Pair],
    // `<name of to>()` written in `from`: the same unit, or an imported one.
    bare: Set[Pair],
    // `use Shop.M<to>` in the file of unit `from`.
    imports: Set[Pair],
    // `%Shop.M<to>{}` or `belongs_to :x, Shop.M<to>` in definition `from`.
    shapes: Set[Pair],
    // Route id -> path pattern (0 is a `:param`), and the action it serves.
    routes: int -> List[int],
    serves: int -> int,
    // A path literal in definition `id` (0 is an interpolated segment).
    requests: Set[Request],
    // Definitions that list the application's modules at run time.
    reflects: Set[int],
    // Run-time edges the analysis cannot see (the named carve-outs).
    preloads: Set[Pair],      // `Repo.preload(x, :assoc)`: from runs the code of unit `to`
    builtPaths: Set[Pair],    // a request path built at run time: from reaches route `to`
    computed: Set[Pair],      // a module chosen by a computed name: from runs definition `to`
    // Message tags, used only by the callbackClauses rule (#347).
    accepts: int -> int,
    sends: Set[Send],
    sendsAny: Set[int],
  }

  type Delta = {
    calls: Set[Pair], bare: Set[Pair], imports: Set[Pair], shapes: Set[Pair],
    requests: Set[Request], reflects: Set[int],
    preloads: Set[Pair], builtPaths: Set[Pair], computed: Set[Pair],
  }
  type Rules = {
    callbackClauses: bool, deadSilent: bool, processStarters: bool, fullFallback: bool,
    schemaTop: bool, routeNothing: bool, sendSites: bool,
  }
  // `comments`: units whose standalone comment line is edited. `trailing`:
  // definitions whose trailing comment (`0 # note`) is edited and nothing else.
  type Changes = { edits: Set[int], comments: Set[int], trailing: Set[int] }
  type Selection = { full: bool, tests: Set[int] }
  type State = { repo: Repo, rules: Rules, changes: Changes, selection: Selection }

  pure val mainRules: Rules = {
    callbackClauses: true, deadSilent: true, processStarters: true, fullFallback: false,
    schemaTop: false, routeNothing: false, sendSites: true,
  }
  pure val noChanges: Changes = { edits: Set(), comments: Set(), trailing: Set() }
  pure val noSelection: Selection = { full: false, tests: Set() }
  pure val emptyRepo: Repo = {
    kindOf: Map(), unitOf: Map(), nameOf: Map(), testUnits: Set(),
    calls: Set(), bare: Set(), imports: Set(), shapes: Set(),
    routes: Map(), serves: Map(), requests: Set(), reflects: Set(),
    preloads: Set(), builtPaths: Set(), computed: Set(),
    accepts: Map(), sends: Set(), sendsAny: Set(),
  }

  // ---- derived relations ----------------------------------------------------

  pure def nodes(r: Repo): Set[int] = r.kindOf.keys()
  pure def kind(r: Repo, d: int): str = r.kindOf.get(d)
  pure def unit(r: Repo, d: int): int = r.unitOf.get(d)
  pure def inTestFile(r: Repo, d: int): bool = r.testUnits.contains(unit(r, d))
  pure def defsOf(r: Repo, u: int): Set[int] = nodes(r).filter(d => unit(r, d) == u)
  pure def topsOf(r: Repo, u: int): Set[int] = defsOf(r, u).filter(d => kind(r, d) == "top")

  // Every reference that names a function: a qualified call, a bare call, and a
  // route naming the action it serves (the route holds the action as an atom).
  pure def callEdges(r: Repo): Set[Pair] =
    r.calls.union(r.bare).union(r.routes.keys().map(id => { from: id, to: r.serves.get(id) }))

  pure def importsUnit(r: Repo, e: int, u: int): bool =
    r.imports.contains({ from: unit(r, e), to: u })

  // `e` uses module `u` by calling into it, or through `use`.
  pure def callsInto(r: Repo, e: int, u: int): bool =
    unit(r, e) != u and
    (callEdges(r).exists(c => c.from == e and unit(r, c.to) == u) or importsUnit(r, e, u))

  // `e` uses module `u` only for the shape of its data.
  pure def shapesOf(r: Repo, e: int, u: int): bool =
    unit(r, e) != u and r.shapes.contains({ from: e, to: u })

  // `e` mentions unit `u` in some way the index records: a call into it, `use`, a
  // struct, or being a definition of it.
  pure def mentions(r: Repo, e: int, u: int): bool =
    unit(r, e) == u or callsInto(r, e, u) or shapesOf(r, e, u)

  // Definitions that name `d` so that a change to `d` reaches them. A function name
  // held as an atom (`apply(mod, :name, args)`) counts when the definition also
  // mentions the function's module.
  pure def namers(r: Repo, d: int): Set[int] =
    nodes(r).filter(e => e != d and
      (callEdges(r).contains({ from: e, to: d }) or
       (r.computed.contains({ from: e, to: d }) and mentions(r, e, unit(r, d)))))

  // Path literal matching: the same segment rules as `facts::path_reaches`.
  pure def pathReaches(lit: List[int], pat: List[int]): bool = {
    val ll = lit.length()
    val pl = pat.length()
    if (ll > pl) false
    else {
      val agree = lit.indices().forall(i => pat[i] == 0 or lit[i] == 0 or lit[i] == pat[i])
      if (not(agree)) false
      else if (ll >= pl) true
      else lit.indices().filter(i => lit[i] != 0).size() >= 2
    }
  }

  pure def requesters(r: Repo, route: int): Set[int] =
    nodes(r).filter(c =>
      c != route and r.requests.exists(q => q.id == c and pathReaches(q.path, r.routes.get(route))))

  // ---- the rules ------------------------------------------------------------
  //
  // Each rule is a guard on a definition that yields Links labelled with the
  // rule that fired. `usersOf` is the unit-wide fan-out.

  // Functions that start a process, or hand a module to one that will.
  pure val startFunctions: Set[str] = Set(
    "start", "start_link", "start_child", "start_supervised", "start_supervised!",
    "start_link_supervised!", "child_spec", "init")
  // Callbacks of a process's life, and the ones that take a message.
  pure val lifecycle: Set[str] = Set("init", "terminate", "code_change", "start_link", "start", "child_spec")
  pure val messageCallbacks: Set[str] = Set("handle_call", "handle_cast", "handle_info", "handle_continue")

  // Qualified calls `e` makes into unit `u`.
  pure def intoCalls(r: Repo, e: int, u: int): Set[Pair] =
    r.calls.filter(c => c.from == e and unit(r, c.to) == u)

  // `e` names unit `u` other than as the receiver of a call to one of its functions: a
  // struct, an import, or no call into it at all.
  pure def namesOtherwise(r: Repo, e: int, u: int): bool =
    shapesOf(r, e, u) or importsUnit(r, e, u) or intoCalls(r, e, u).size() == 0

  // `e` may start a process of unit `u`, which runs its `init` and `terminate`.
  pure def mayStart(r: Repo, e: int, u: int): bool =
    unit(r, e) != u and
    (intoCalls(r, e, u).exists(c => startFunctions.contains(r.nameOf.get(c.to)) or kind(r, c.to) == "macro")
     or namesOtherwise(r, e, u))

  // `e` may send a message tagged `tag` (any message when `tag` is 0) to a process of unit `u`.
  // With `sendSites`, only a macro call or an import counts as naming the unit in a way that
  // may send: a struct (like a child specification) names it without sending. Handing the
  // module to a call, which the engine also counts, is not modelled.
  pure def maySend(r: Repo, rules: Rules, e: int, u: int, tag: int): bool = {
    val sent = r.sends.filter(s => s.id == e)
    r.sendsAny.contains(e) or
    (if (tag == 0) sent.size() > 0 else sent.exists(s => s.tag == tag)) or
    ((tag == 0 or sent.size() == 0) and unit(r, e) != u and
     (intoCalls(r, e, u).exists(c => kind(r, c.to) == "macro") or
      (if (rules.sendSites) importsUnit(r, e, u) else namesOtherwise(r, e, u))))
  }

  // Definitions that use unit `u`, as seen from `d`, and so are reached when `d` fans out.
  pure def usersOf(r: Repo, rules: Rules, u: int, d: int): Set[int] = {
    val called = nodes(r).filter(e => callsInto(r, e, u))
    val shaped = nodes(r).filter(e => shapesOf(r, e, u) and not(callsInto(r, e, u)))
    // #348: a shape-only use depends on the target's top-level code, not its functions.
    val shapeReached = if (rules.schemaTop and kind(r, d) != "top") Set() else shaped
    called.union(shapeReached)
  }

  // Where a callback fans out to. #347: a clause with a literal message tag goes to
  // senders of it. #353: a lifecycle callback goes to what may start the process, and a
  // clause that matches any message to what may send one or start the process.
  pure def callbackUsers(r: Repo, rules: Rules, u: int, d: int): Set[int] = {
    val tag = if (r.accepts.keys().contains(d)) r.accepts.get(d) else 0
    val name = r.nameOf.get(d)
    val candidates = usersOf(r, rules, u, d).union(defsOf(r, u)).exclude(Set(d))
    if (rules.callbackClauses and tag > 0) candidates.filter(e => maySend(r, rules, e, u, tag))
    else if (rules.processStarters and lifecycle.contains(name)) candidates.filter(e => mayStart(r, e, u))
    else if (rules.processStarters and messageCallbacks.contains(name))
      candidates.filter(e => maySend(r, rules, e, u, 0) or mayStart(r, e, u))
    else usersOf(r, rules, u, d)
  }

  // A file that registers no callbacks (`use`, `@behaviour`, ...) cannot have a function
  // called by a name nothing writes.
  pure def registers(r: Repo, u: int): bool = r.imports.exists(i => i.from == u)

  // The rule that sends `d` to every user of its unit, or "". `edited`: definitions the
  // change itself touches, which keep the unnamed rule.
  pure def fanRule(r: Repo, rules: Rules, edited: Set[int], d: int): str =
    if (inTestFile(r, d)) ""
    else if (kind(r, d) == "callback") "callback"
    else if (kind(r, d) == "macro") "macro"
    else if (kind(r, d) == "fn" and namers(r, d).size() == 0 and
             not(rules.deadSilent and not(registers(r, unit(r, d))) and not(edited.contains(d)))) "unnamed"
    else ""

  pure def mk(from: int, tos: Set[int], rule: str): Set[Link] =
    tos.map(t => { from: from, to: t, rule: rule })

  // Everything a reached definition `d` reaches next.
  pure def linksFrom(r: Repo, rules: Rules, edited: Set[int], d: int): Set[Link] = {
    val u = unit(r, d)
    val k = kind(r, d)
    if (k == "test") Set()
    else if (k == "top")
      // The file's top-level code is part of every definition, and of every user.
      mk(d, defsOf(r, u).exclude(Set(d)), "top-members")
        .union(mk(d, usersOf(r, rules, u, d), "top-fanout"))
    else if (k == "route") {
      val hits = requesters(r, d)
      if (hits.size() > 0) mk(d, hits, "route-request")
      else if (rules.routeNothing) Set()
      else mk(d, usersOf(r, rules, u, d), "route-fallback")
    } else {
      val fan = fanRule(r, rules, edited, d)
      mk(d, namers(r, d), "named")
        .union(if (fan == "") Set()
               else if (kind(r, d) == "callback") mk(d, callbackUsers(r, rules, u, d), fan)
               else mk(d, usersOf(r, rules, u, d), fan))
    }
  }

  pure def stepLinks(r: Repo, rules: Rules, edited: Set[int], from: Set[int]): Set[Link] =
    from.fold(Set(), (acc, d) => acc.union(linksFrom(r, rules, edited, d)))

  type Reach = { nodes: Set[int], links: Set[Link] }

  // The least fixpoint of `linksFrom` from `seeds`.
  pure def reach(r: Repo, rules: Rules, seeds: Set[int]): Reach =
    nodes(r).fold({ nodes: seeds, links: Set() }, (acc, _) => {
      val ls = stepLinks(r, rules, seeds, acc.nodes)
      { nodes: acc.nodes.union(ls.map(l => l.to)), links: acc.links.union(ls) }
    })

  // ---- selection ------------------------------------------------------------

  // The index reads a trailing comment as part of the code line it ends, so editing
  // one counts as editing the definition.
  pure def touched(ch: Changes): Set[int] = ch.edits.union(ch.trailing)

  // Code that lists modules at run time reaches whatever a non-test file changes.
  pure def reflectRoots(r: Repo, ch: Changes): Set[int] =
    if (touched(ch).exists(d => not(inTestFile(r, d)))) r.reflects else Set()

  pure def seeds(r: Repo, ch: Changes): Set[int] = touched(ch).union(reflectRoots(r, ch))

  pure def reachedTests(r: Repo, nodeSet: Set[int]): Set[int] =
    nodeSet.filter(d => inTestFile(r, d)).map(d => unit(r, d))

  // A changed non-test file whose edits alone reach no test "is not understood":
  // the project runs in full. The check does not see reflection.
  pure def unreached(r: Repo, rules: Rules, ch: Changes): bool =
    touched(ch).map(d => unit(r, d)).filter(u => not(r.testUnits.contains(u))).exists(u =>
      reachedTests(r, reach(r, rules, touched(ch).filter(d => unit(r, d) == u)).nodes).size() == 0)

  pure def selectTests(r: Repo, rules: Rules, ch: Changes): Selection = {
    val full = rules.fullFallback and unreached(r, rules, ch)
    val got = reachedTests(r, reach(r, rules, seeds(r, ch)).nodes)
    { full: full, tests: if (full) r.testUnits else got }
  }

  // ---- the run-time oracle --------------------------------------------------
  //
  // What may execute what, written from how Elixir runs code and not from the
  // analysis. Kinds in `visibleKinds` are what the analysis claims to follow;
  // the rest are the named carve-outs.

  pure val visibleKinds: Set[str] = Set("call", "callback", "macro", "top", "route", "reflect")
  pure val carveOuts: Set[str] = Set("preload", "built-path", "computed")

  pure def execEdges(r: Repo): Set[Exec] = {
    val ds = nodes(r)
    val calls = callEdges(r).map(c => { from: c.from, to: c.to, kind: "call" })
    // The runtime invokes a callback of every module a definition calls into.
    val callbacks = ds.filter(b => kind(r, b) == "callback").map(b =>
      ds.filter(e => callsInto(r, e, unit(r, b))).map(e => { from: e, to: b, kind: "callback" })).flatten()
    // A macro expands in every definition that calls it or imports its module.
    val macros = ds.filter(b => kind(r, b) == "macro").map(b =>
      ds.filter(e => unit(r, e) != unit(r, b) and importsUnit(r, e, unit(r, b)))
        .map(e => { from: e, to: b, kind: "macro" })).flatten()
    // Running code of a module runs its top-level code; so does a struct built from it.
    val tops = ds.filter(t => kind(r, t) == "top").map(t =>
      ds.filter(e => unit(r, e) == unit(r, t) or callsInto(r, e, unit(r, t)) or shapesOf(r, e, unit(r, t)))
        .exclude(Set(t)).map(e => { from: e, to: t, kind: "top" })).flatten()
    val routes = r.routes.keys().map(rt =>
      ds.filter(e => e != rt and r.requests.exists(q => q.id == e and pathReaches(q.path, r.routes.get(rt))))
        .map(e => { from: e, to: rt, kind: "route" })).flatten()
    val reflect = r.reflects.map(e => ds.exclude(Set(e)).map(d => { from: e, to: d, kind: "reflect" })).flatten()
    val preload = r.preloads.map(p => defsOf(r, p.to).map(d => { from: p.from, to: d, kind: "preload" })).flatten()
    val built = r.builtPaths.map(p => Set({ from: p.from, to: p.to, kind: "built-path" }))
    val computed = r.computed.map(p => Set({ from: p.from, to: p.to, kind: "computed" }))
    calls.union(callbacks).union(macros).union(tops).union(routes).union(reflect)
      .union(preload.union(built.flatten()).union(computed.flatten()))
  }

  // Definitions `from` may execute, following only `kinds`.
  pure def runs(r: Repo, kinds: Set[str], from: int): Set[int] = {
    val es = execEdges(r).filter(e => kinds.contains(e.kind))
    nodes(r).fold(Set(from), (acc, _) =>
      acc.union(es.filter(e => acc.contains(e.from)).map(e => e.to)))
  }

  pure def allKinds: Set[str] = visibleKinds.union(carveOuts)

  // ---- properties -----------------------------------------------------------

  pure def testDefs(r: Repo): Set[int] = nodes(r).filter(d => inTestFile(r, d))

  // Soundness: a test that may run changed code through a kind the analysis follows is selected.
  pure def sound(r: Repo, sel: Selection, ch: Changes): bool =
    sel.full or testDefs(r).forall(t =>
      ch.edits.forall(c => runs(r, visibleKinds, t).contains(c) implies sel.tests.contains(unit(r, t))))

  // Tests that may run changed code, and are not selected, only through a carve-out.
  pure def missedByCarveOut(r: Repo, sel: Selection, ch: Changes): Set[int] =
    if (sel.full) Set()
    else testDefs(r).filter(t =>
      not(sel.tests.contains(unit(r, t))) and ch.edits.exists(c => runs(r, allKinds, t).contains(c)))

  // Precision: every node the walk reached is a seed or the target of a link, and
  // every link's source was reached, so each selected test has a witness path.
  pure def witnessed(r: Repo, rules: Rules, ch: Changes): bool = {
    val rc = reach(r, rules, seeds(r, ch))
    val fromSeeds = nodes(r).fold(seeds(r, ch), (acc, _) =>
      acc.union(rc.links.filter(l => acc.contains(l.from)).map(l => l.to)))
    fromSeeds == rc.nodes and rc.links.forall(l => rc.nodes.contains(l.from))
  }

  pure val ruleNames: Set[str] =
    Set("named", "callback", "macro", "unnamed", "top-members", "top-fanout", "route-request", "route-fallback")
  // The rules that send a definition to every user of its unit.
  pure val fanoutRules: Set[str] = Set("callback", "macro", "unnamed", "top-fanout", "route-fallback")
  // The unit-wide fan-out the stated invariant allows.
  pure val statedFanout: Set[str] = Set("callback", "macro", "unnamed")

  // Every link carries a modelled rule, and a fan-out link comes from a definition of the right kind.
  pure def linksJustified(r: Repo, rules: Rules, ch: Changes): bool =
    reach(r, rules, seeds(r, ch)).links.forall(l =>
      ruleNames.contains(l.rule) and
      (l.rule == "callback" implies kind(r, l.from) == "callback") and
      (l.rule == "macro" implies kind(r, l.from) == "macro") and
      (l.rule == "unnamed" implies (kind(r, l.from) == "fn" and namers(r, l.from).size() == 0)) and
      (l.rule == "top-fanout" implies kind(r, l.from) == "top") and
      (l.rule == "route-fallback" implies (kind(r, l.from) == "route" and requesters(r, l.from).size() == 0)))

  // The fan-out the stated invariant does not list, that fired.
  pure def unstatedFanout(r: Repo, rules: Rules, ch: Changes): Set[str] =
    reach(r, rules, seeds(r, ch)).links.filter(l => fanoutRules.contains(l.rule) and not(statedFanout.contains(l.rule)))
      .map(l => l.rule)

  pure def contained(a: Selection, b: Selection): bool =
    (a.full implies b.full) and a.tests.subseteq(b.tests)

  // Superset of edits selects a superset of tests.
  pure def monotone(r: Repo, rules: Rules, small: Changes, big: Changes): bool =
    touched(small).subseteq(touched(big)) implies
      contained(selectTests(r, rules, small), selectTests(r, rules, big))

  // The same, when the smaller change is not a full run. A full run is the
  // fallback for a change that "reaches no test"; adding an edit that does reach
  // one narrows it, which `monotone` forbids and the engine does.
  pure def monotonePrecise(r: Repo, rules: Rules, small: Changes, big: Changes): bool =
    (touched(small).subseteq(touched(big)) and not(selectTests(r, rules, small).full)) implies
      contained(selectTests(r, rules, small), selectTests(r, rules, big))

  // Wellformedness of a repo constant.
  pure def wellFormed(r: Repo): bool = {
    val ds = nodes(r)
    r.unitOf.keys() == ds and r.nameOf.keys() == ds and
    r.calls.union(r.bare).union(r.shapes).union(r.preloads).union(r.builtPaths).union(r.computed)
      .forall(p => ds.contains(p.from)) and
    r.calls.union(r.bare).union(r.computed).forall(p => ds.contains(p.to)) and
    // A bare call goes to the same unit or an imported one.
    r.bare.forall(p => unit(r, p.from) == unit(r, p.to) or importsUnit(r, p.from, unit(r, p.to))) and
    // Routes name a function, live in a non-test file, and are made of route kind.
    r.routes.keys().forall(id => kind(r, id) == "route" and ds.contains(r.serves.get(id))) and
    ds.filter(d => kind(r, d) == "route") == r.routes.keys() and
    // Test files hold only tests; at most one top per unit; names unique within a unit.
    ds.forall(d => inTestFile(r, d) iff kind(r, d) == "test") and
    r.testUnits.forall(u => topsOf(r, u).size() == 0) and
    r.unitOf.keys().forall(d => topsOf(r, unit(r, d)).size() <= 1) and
    ds.forall(a => ds.forall(b => (a != b and unit(r, a) == unit(r, b)) implies r.nameOf.get(a) != r.nameOf.get(b))) and
    // A request or reflection lives in some definition.
    r.requests.forall(q => ds.contains(q.id) and q.path.length() > 0) and
    r.reflects.subseteq(ds)
  }

  // ---- the state machine the engine is checked against ----------------------

  var state: State

  action init = all {
    state' = { repo: emptyRepo, rules: mainRules, changes: noChanges, selection: noSelection },
  }

  // Lay a repo out as files and commit it.
  action build(repo: Repo, rules: Rules): bool = all {
    state' = { repo: repo, rules: rules, changes: noChanges, selection: noSelection },
  }

  // The references of a generated repo, laid over a base repo's definitions.
  // (A constant this small keeps the compiled scenarios small.)
  action buildWith(base: Repo, delta: Delta, rules: Rules): bool = all {
    state' = {
      repo: base.with("calls", delta.calls).with("bare", delta.bare).with("imports", delta.imports)
        .with("shapes", delta.shapes).with("requests", delta.requests).with("reflects", delta.reflects)
        .with("preloads", delta.preloads).with("builtPaths", delta.builtPaths)
        .with("computed", delta.computed),
      rules: rules, changes: noChanges, selection: noSelection,
    },
  }

  // Edit definitions and comments in the working tree.
  action change(ch: Changes): bool = all {
    state' = state.with("changes", ch).with("selection", noSelection),
  }

  // Ask the engine which tests the change selects; `expected` is what the model
  // says (`selectTests` of the same repo, rules and changes). It is passed in, as
  // a constant Quint has evaluated, so that the checker compares the engine with
  // it instead of re-evaluating the selection function itself.
  action selectNow(expected: Selection): bool = all {
    state' = state.with("selection", expected),
  }
}
