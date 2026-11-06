// Where the rules meet the stated invariants, each pinned to a fixed repo. Some
// are divergences the model found that main has since fixed (D1, D3, D5, D7:
// #353 and #354); they are written as the behaviour that now holds, with the old
// rule one switch away. The rest still hold on main. If one fails, the engine's
// rule changed and the model's switches need updating.
//
// Run with `quint test --main witnesses --match 'Witness$' witnesses.qnt`.
module witnesses {
  import related.* from "./model"
  import repos.* from "./repos"

  pure def editing(ds: Set[int]): Changes = { edits: ds, comments: Set(), trailing: Set() }
  pure val nothing: Selection = { full: false, tests: Set() }
  // Main with one open pull request applied.
  pure val pr349: Rules = mainRules.with("routeNothing", true)
  pure val pr348: Rules = mainRules.with("schemaTop", true)
  // Main before each merged pull request.
  pure val before347: Rules = mainRules.with("callbackClauses", false)
  pure val before353: Rules = mainRules.with("deadSilent", false).with("processStarters", false)
  pure val before354: Rules = mainRules.with("fullFallback", true)
  pure val beforeSendSites: Rules = mainRules.with("sendSites", false)

  // D1 (fixed by #354). A superset of edits selects a superset of tests. With the
  // "reaches no test, run in full" check, the larger change selected fewer.
  run monotoneWitness = init.then({
    val small = editing(Set(90))
    val big = editing(Set(90, 91))
    all {
      assert(selectTests(shopQuiet, mainRules, small) == nothing),
      assert(selectTests(shopQuiet, mainRules, big) == { full: false, tests: Set(110) }),
      assert(monotone(shopQuiet, mainRules, small, big)),
      assert(selectTests(shopQuiet, before354, small).full),
      assert(not(monotone(shopQuiet, before354, small, big))),
      state' = state,
    }
  })

  // D2. Unit-wide fan-out outside "callback, macro or unnamed": top-level code, and a route
  // nothing requests. Both still fire on main.
  run unstatedFanoutWitness = init.then(all {
    assert(unstatedFanout(shopQuiet, mainRules, editing(Set(13))) == Set("top-fanout")),
    assert(unstatedFanout(shopQuiet, mainRules, editing(Set(81))) == Set("route-fallback")),
    assert(unstatedFanout(shopQuiet, mainRules, editing(Set(10))) == Set()),
    state' = state,
  })

  // D3 (fixed by #354). With #349's rule an unrequested route reaches nothing, and the change
  // selects nothing; before #354 the fallback turned that into a full run.
  run routeNothingWitness = init.then(all {
    assert(selectTests(shopQuiet, mainRules, editing(Set(81))) == { full: false, tests: Set(109) }),
    assert(selectTests(shopQuiet, pr349, editing(Set(81))) == nothing),
    assert(selectTests(shopQuiet, pr349.with("fullFallback", true), editing(Set(81))).full),
    state' = state,
  })

  // D5 (fixed by #354). The reflecting test is selected for a change nothing else reaches,
  // and there is no fallback to a full run.
  run reflectionWitness = init.then(all {
    assert(selectTests(shop, mainRules, editing(Set(90))) == { full: false, tests: Set(105) }),
    assert(selectTests(shop, before354, editing(Set(90))).full),
    state' = state,
  })

  // #353. A function nothing calls, in a file that registers no callbacks, reaches nothing
  // unless it is the edit; lifecycle callbacks are reached by what may start the process.
  run deadFunctionWitness = init.then(all {
    // `unused` is edited: it keeps the unnamed rule and reaches the users of Pricing.
    assert(selectTests(shopQuiet, mainRules, editing(Set(12))).tests == Set(101)),
    // `blank` (builds %Order{}) is dead: editing `legacy` no longer reaches the tests of Cart.
    assert(selectTests(shopQuiet, before353, editing(Set(52))).tests == Set(101, 106, 108)),
    assert(selectTests(shopQuiet, mainRules, editing(Set(52))).tests == Set(108)),
    state' = state,
  })

  // #348 (open). Struct users wait for the target's top-level code. On the shop the struct
  // users are dead functions, so main already selects the same tests.
  run schemaShapeWitness = init.then(all {
    assert(selectTests(shopQuiet, pr348, editing(Set(52))) == selectTests(shopQuiet, mainRules, editing(Set(52)))),
    assert(selectTests(shopQuiet, pr348, editing(Set(51))).tests.contains(108)),
    state' = state,
  })

  // D6. Carve-outs: tests that run changed code through preload, a built path or a computed name are not selected.
  run carveOutWitness = init.then(all {
    assert(missedByCarveOut(shopQuiet, selectTests(shopQuiet, mainRules, editing(Set(62))), editing(Set(62))) == Set(1070)),
    assert(missedByCarveOut(shopQuiet, selectTests(shopQuiet, mainRules, editing(Set(80))), editing(Set(80))) == Set(1070)),
    assert(missedByCarveOut(shopQuiet, selectTests(shopQuiet, mainRules, editing(Set(91))), editing(Set(91))) == Set(1070)),
    state' = state,
  })

  // D7 (#347 and #354). A clause with a literal tag is reached by senders of that tag and of
  // computed messages. When nobody sends the tag the change selects nothing; before #354 it ran in full.
  run messageClauseWitness = init.then(all {
    val quietWorkers = workers.with("sendsAny", Set())
    all {
      assert(selectTests(workers, before347, editing(Set(31))).tests == Set(102, 111, 112, 113)),
      assert(selectTests(workers, mainRules, editing(Set(31))).tests == Set(102, 112)),
      assert(selectTests(workers, mainRules, editing(Set(33))).tests == Set(112)),
      // Sending sites: a test that only names the module (unit 113) was a possible sender.
      assert(selectTests(workers, beforeSendSites, editing(Set(31))).tests == Set(102, 112, 113)),
      assert(selectTests(workers, beforeSendSites, editing(Set(33))).tests == Set(112, 113)),
      assert(selectTests(quietWorkers, mainRules, editing(Set(33))) == nothing),
      assert(selectTests(quietWorkers, before354, editing(Set(33))).full),
      state' = state,
    }
  })

  // D8. A change to a trailing comment alone selects tests: the line it ends is code.
  run trailingCommentWitness = init.then(all {
    val ch = { edits: Set(), comments: Set(), trailing: Set(10) }
    all {
      assert(selectTests(shopQuiet, mainRules, ch) == { full: false, tests: Set(101) }),
      assert(selectTests(shopQuiet, mainRules, { edits: Set(), comments: Set(1), trailing: Set() }) == nothing),
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
