// Divergences the model finds between the rules and the stated invariants,
// each pinned to a fixed repo. These pass while the divergence exists: if one
// fails, the engine's rule changed and the PR description needs updating.
//
// Run with `quint test --main witnesses --match 'Witness$' witnesses.qnt`.
module witnesses {
  import related.* from "./model"
  import repos.* from "./repos"

  pure def editing(ds: Set[int]): Changes = { edits: ds, comments: Set(), trailing: Set() }
  pure val everything: Selection = { full: true, tests: shop.testUnits }
  pure val pr349: Rules = { callbackClauses: true, schemaTop: false, routeNothing: true }
  // Main before #347 merged.
  pure val before347: Rules = { callbackClauses: false, schemaTop: false, routeNothing: false }
  pure val pr348: Rules = { callbackClauses: true, schemaTop: true, routeNothing: false }

  // D1. Monotonic fails: adding an edit that reaches a test turns a full run into a narrow one.
  run monotoneWitness = init.then({
    val small = editing(Set(90))
    val big = editing(Set(90, 91))
    all {
      assert(selectTests(shopQuiet, mainRules, small) == everything),
      assert(selectTests(shopQuiet, mainRules, big) == { full: false, tests: Set(110) }),
      assert(not(monotone(shopQuiet, mainRules, small, big))),
      assert(monotonePrecise(shopQuiet, mainRules, small, big)),
      state' = state,
    }
  })

  // D2. Unit-wide fan-out outside "callback, macro or unnamed": top-level code, and a route nothing requests.
  run unstatedFanoutWitness = init.then(all {
    assert(unstatedFanout(shopQuiet, mainRules, editing(Set(13))) == Set("top-fanout")),
    assert(unstatedFanout(shopQuiet, mainRules, editing(Set(81))) == Set("route-fallback")),
    assert(unstatedFanout(shopQuiet, mainRules, editing(Set(10))) == Set()),
    state' = state,
  })

  // D3. The fallback for a change that reaches no test is a full run, so the route rule of
  // #349 does not select "nothing" for an unrequested route: the file then reaches no test.
  run routeNothingWitness = init.then(all {
    assert(selectTests(shopQuiet, mainRules, editing(Set(81))) == { full: false, tests: Set(109) }),
    assert(selectTests(shopQuiet, pr349, editing(Set(81))) == everything),
    state' = state,
  })

  // D4. Reflection reaches everything, so a change that no test names still selects the reflecting test
  // and still falls back to a full run: the reflecting test is not counted when asking whether a change reaches a test.
  run reflectionDoesNotCountWitness = init.then(all {
    assert(selectTests(shop, mainRules, editing(Set(90))) == everything),
    assert(reachedTests(shop, reach(shop, mainRules, seeds(shop, editing(Set(90)))).nodes).contains(105)),
    state' = state,
  })

  // D5. Schema shape (#348): a function change reaches a struct user on main, not once shape-only uses wait for the top.
  run schemaShapeWitness = init.then(all {
    assert(selectTests(shopQuiet, mainRules, editing(Set(52))).tests == Set(101, 106, 108)),
    assert(selectTests(shopQuiet, pr348, editing(Set(52))).tests == Set(108)),
    // The unit's top-level code still reaches the shape user.
    assert(selectTests(shopQuiet, pr348, editing(Set(51))).tests.contains(101)),
    state' = state,
  })

  // D6. Carve-outs: tests that run changed code through preload, a built path or a computed name are not selected.
  run carveOutWitness = init.then(all {
    assert(missedByCarveOut(shopQuiet, selectTests(shopQuiet, mainRules, editing(Set(62))), editing(Set(62))) == Set(1070)),
    assert(missedByCarveOut(shopQuiet, selectTests(shopQuiet, mainRules, editing(Set(80))), editing(Set(80))) == Set(1070)),
    assert(missedByCarveOut(shopQuiet, selectTests(shopQuiet, mainRules, editing(Set(91))), editing(Set(91))) == Set(1070)),
    state' = state,
  })

  // D7. Message clauses (#347, merged): a clause with a literal tag is reached by senders of that tag
  // (and senders of computed messages), not by every user of the module.
  run messageClauseWitness = init.then(all {
    assert(selectTests(workers, before347, editing(Set(31))).tests == Set(102, 111, 112)),
    assert(selectTests(workers, mainRules, editing(Set(31))).tests == Set(102, 112)),
    assert(selectTests(workers, mainRules, editing(Set(33))).tests == Set(112)),
    state' = state,
  })

  // D8. A change to a trailing comment alone selects tests: the line it ends is code.
  run trailingCommentWitness = init.then(all {
    val ch = { edits: Set(), comments: Set(), trailing: Set(10) }
    all {
      assert(selectTests(shopQuiet, mainRules, ch) == { full: false, tests: Set(101) }),
      assert(selectTests(shopQuiet, mainRules, { edits: Set(), comments: Set(1), trailing: Set() }) == { full: false, tests: Set() }),
      state' = state,
    }
  })

  // D9. Each of the three rules (#347 is on main now, #348 and #349 are open) removes a fan-out, and a change whose fan-out is
  // now empty "reaches no test", so the fallback turns a narrow selection into a full
  // run. Customer is reached only by a struct user; nothing sends tag 6.
  run narrowedToNothingRunsEverythingWitness = init.then(all {
    val quietWorkers = workers.with("sendsAny", Set())
    all {
      assert(selectTests(shopQuiet, mainRules, editing(Set(62))).full == false),
      assert(selectTests(shopQuiet, pr348, editing(Set(62))) == everything),
      assert(selectTests(quietWorkers, before347, editing(Set(33))).full == false),
      assert(selectTests(quietWorkers, mainRules, editing(Set(33))).full),
      assert(selectTests(shopQuiet, mainRules, editing(Set(81))).full == false),
      assert(selectTests(shopQuiet, pr349, editing(Set(81))).full),
      state' = state,
    }
  })

  // The scenario repos are well formed.
  run repoWitness = init.then(all {
    assert(wellFormed(shop)),
    assert(wellFormed(shopQuiet)),
    assert(wellFormed(workers)),
    state' = state,
  })
}
