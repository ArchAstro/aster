// Scenarios the engine is run on. Each one builds `shop` (or `shopQuiet`) as
// files in a git repo, edits it, and asks `aster` which tests the edit selects.
// The assertions state what the model says; the conformance run checks that the
// engine says the same.
module scenarios {
  import related.* from "./model"
  import repos.* from "./repos"
  import universe.* from "./universe"

  pure def editing(ds: Set[int]): Changes = { edits: ds, comments: Set(), trailing: Set() }
  pure val edit10: Changes = editing(Set(10))
  pure val edit10_31: Changes = editing(Set(10, 31))
  pure val edit12: Changes = editing(Set(12))
  pure val edit13: Changes = editing(Set(13))
  pure val edit21: Changes = editing(Set(21))
  pure val edit31: Changes = editing(Set(31))
  pure val edit40: Changes = editing(Set(40))
  pure val edit52: Changes = editing(Set(52))
  pure val edit62: Changes = editing(Set(62))
  pure val edit80: Changes = editing(Set(80))
  pure val edit81: Changes = editing(Set(81))
  pure val edit90: Changes = editing(Set(90))
  pure val edit90_91: Changes = editing(Set(90, 91))
  pure val edit91: Changes = editing(Set(91))
  pure val edit1010: Changes = editing(Set(1010))
  pure val commentOnly: Changes = { edits: Set(), comments: Set(1, 2, 101), trailing: Set() }
  pure def picked(ts: Set[int]): Selection = { full: false, tests: ts }

  pure val expect_shopQuiet_commentOnly: Selection = selectTests(shopQuiet, mainRules, commentOnly)
  pure val expect_shopQuiet_edit10: Selection = selectTests(shopQuiet, mainRules, edit10)
  pure val expect_shopQuiet_edit12: Selection = selectTests(shopQuiet, mainRules, edit12)
  pure val expect_shopQuiet_edit31: Selection = selectTests(shopQuiet, mainRules, edit31)
  pure val expect_shopQuiet_edit40: Selection = selectTests(shopQuiet, mainRules, edit40)
  pure val expect_shopQuiet_edit13: Selection = selectTests(shopQuiet, mainRules, edit13)
  pure val expect_shopQuiet_edit52: Selection = selectTests(shopQuiet, mainRules, edit52)
  pure val expect_shopQuiet_edit80: Selection = selectTests(shopQuiet, mainRules, edit80)
  pure val expect_shopQuiet_edit81: Selection = selectTests(shopQuiet, mainRules, edit81)
  pure val expect_shop_edit21: Selection = selectTests(shop, mainRules, edit21)
  pure val expect_shopQuiet_edit1010: Selection = selectTests(shopQuiet, mainRules, edit1010)
  pure val expect_shopQuiet_edit10_31: Selection = selectTests(shopQuiet, mainRules, edit10_31)
  pure val expect_shopQuiet_edit90: Selection = selectTests(shopQuiet, mainRules, edit90)
  pure val expect_shopQuiet_edit90_91: Selection = selectTests(shopQuiet, mainRules, edit90_91)
  pure val expect_shopQuiet_edit62: Selection = selectTests(shopQuiet, mainRules, edit62)
  pure val expect_shopQuiet_edit91: Selection = selectTests(shopQuiet, mainRules, edit91)

  // A comment edit selects nothing.
  /// @conformance
  run commentOnlyRun = init.then(build(shopQuiet, mainRules)).then(change(commentOnly)).then(selectNow(expect_shopQuiet_commentOnly))
    .then(all { assert(state.selection == noSelection), state' = state })

  // sum is named by total, which a test calls.
  /// @conformance
  run namedChainRun = init.then(build(shopQuiet, mainRules)).then(change(edit10)).then(selectNow(expect_shopQuiet_edit10))
    .then(all { assert(state.selection == picked(Set(101))), state' = state })

  // Nothing names `unused`, so every user of Pricing is reached: total (which
  // calls Pricing.sum) and the test that calls Pricing.tax.
  /// @conformance
  run unnamedFanoutRun = init.then(build(shopQuiet, mainRules)).then(change(edit12)).then(selectNow(expect_shopQuiet_edit12))
    .then(all { assert(state.selection == picked(Set(101))), state' = state })

  // A callback is invoked by the runtime, so it reaches every user of its module.
  /// @conformance
  run callbackRun = init.then(build(shopQuiet, mainRules)).then(change(edit31)).then(selectNow(expect_shopQuiet_edit31))
    .then(all { assert(state.selection == picked(Set(102))), state' = state })

  // A macro expands where it is used; the test that does `use Tracing` is reached.
  /// @conformance
  run macroRun = init.then(build(shopQuiet, mainRules)).then(change(edit40)).then(selectNow(expect_shopQuiet_edit40))
    .then(all { assert(state.selection == picked(Set(104))), state' = state })

  // Top-level code of Pricing is part of its functions and of its users.
  /// @conformance
  run topLevelRun = init.then(build(shopQuiet, mainRules)).then(change(edit13)).then(selectNow(expect_shopQuiet_edit13))
    .then(all { assert(state.selection == picked(Set(101))), state' = state })

  // `legacy` is edited and nothing names it, so it reaches the users of Order. `blank`, which
  // builds `%Order{}`, is a function nothing calls in a file that registers no callbacks:
  // it reaches nothing.
  /// @conformance
  run schemaShapeRun = init.then(build(shopQuiet, mainRules)).then(change(edit52)).then(selectNow(expect_shopQuiet_edit52))
    .then(all { assert(state.selection == picked(Set(108))), state' = state })

  // The controller action is named by its route; the test that requests the path is selected.
  /// @conformance
  run routeRequestRun = init.then(build(shopQuiet, mainRules)).then(change(edit80)).then(selectNow(expect_shopQuiet_edit80))
    .then(all { assert(state.selection == picked(Set(103))), state' = state })

  // No code requests /s2/:id, so on main the route reaches every user of the router.
  /// @conformance
  run routeFallbackRun = init.then(build(shopQuiet, mainRules)).then(change(edit81)).then(selectNow(expect_shopQuiet_edit81))
    .then(all { assert(state.selection == picked(Set(109))), state' = state })

  // Code that lists modules at run time is reached by any change to a non-test file.
  /// @conformance
  run reflectionRun = init.then(build(shop, mainRules)).then(change(edit21)).then(selectNow(expect_shop_edit21))
    .then(all { assert(state.selection == picked(Set(105, 106))), state' = state })

  // Editing a test selects that test.
  /// @conformance
  run testEditRun = init.then(build(shopQuiet, mainRules)).then(change(edit1010)).then(selectNow(expect_shopQuiet_edit1010))
    .then(all { assert(state.selection == picked(Set(101))), state' = state })

  // Two edits select what each selects.
  /// @conformance
  run twoEditsRun = init.then(build(shopQuiet, mainRules)).then(change(edit10_31)).then(selectNow(expect_shopQuiet_edit10_31))
    .then(all { assert(state.selection == picked(Set(101, 102))), state' = state })

  // helper is reached by `run`, which nothing uses: the change reaches no test and selects nothing.
  /// @conformance
  run deadChangeRun = init.then(build(shopQuiet, mainRules)).then(change(edit90)).then(selectNow(expect_shopQuiet_edit90))
    .then(all { assert(state.selection == noSelection), state' = state })

  // Adding an edit that reaches a test adds that test to the selection and removes none.
  /// @conformance
  run monotoneRun = init.then(build(shopQuiet, mainRules))
    .then(change(edit90)).then(selectNow(expect_shopQuiet_edit90))
    .then(all { assert(state.selection == noSelection), state' = state })
    .then(change(edit90_91)).then(selectNow(expect_shopQuiet_edit90_91))
    .then(all { assert(state.selection == picked(Set(110))), state' = state })

  // Preload, a built path and a computed name run code the analysis cannot see.
  /// @conformance
  run carveOutsRun = init.then(build(shopQuiet, mainRules)).then(change(edit62)).then(selectNow(expect_shopQuiet_edit62))
    .then(all { assert(not(state.selection.tests.contains(107))), state' = state })
    .then(change(edit80)).then(selectNow(expect_shopQuiet_edit80))
    .then(all { assert(state.selection == picked(Set(103))), state' = state })
    .then(change(edit91)).then(selectNow(expect_shopQuiet_edit91))
    .then(all { assert(state.selection == picked(Set(110))), state' = state })

  pure val trailingOnly: Changes = { edits: Set(), comments: Set(), trailing: Set(10) }
  pure val expect_shopQuiet_trailingOnly: Selection = selectTests(shopQuiet, mainRules, trailingOnly)
  pure val edit90_10: Changes = editing(Set(90, 10))
  pure val expect_shopQuiet_edit90_10: Selection = selectTests(shopQuiet, mainRules, edit90_10)

  // Only the trailing comment of a line changes (`0 # note`): the line is code, so the definition is edited.
  /// @conformance
  run trailingCommentRun = init.then(build(shopQuiet, mainRules)).then(change(trailingOnly)).then(selectNow(expect_shopQuiet_trailingOnly))
    .then(all { assert(state.selection == picked(Set(101))), state' = state })

  // A file whose edit reaches no test adds nothing to what the other file reaches.
  /// @conformance
  run twoFilesRun = init.then(build(shopQuiet, mainRules)).then(change(edit90_10)).then(selectNow(expect_shopQuiet_edit90_10))
    .then(all { assert(state.selection == picked(Set(101))), state' = state })

  pure val edit31w: Changes = editing(Set(31))
  pure val expect_workers_edit31: Selection = selectTests(workers, mainRules, edit31w)

  // A message clause with a literal tag is reached by senders of that tag and of computed
  // messages (#347), not by every user of the module.
  /// @conformance
  run messageClauseRun = init.then(build(workers, mainRules)).then(change(edit31w)).then(selectNow(expect_workers_edit31))
    .then(all { assert(state.selection == picked(Set(102, 112))), state' = state })

  // ---- Generated repos -------------------------------------------------------
  // Seeds 1 to 40 of `genRepo` and `genChange` (universe.qnt): the shop with a
  // pseudo-random subset of the references a repo may hold, and a random change.

  pure val d1: Delta = genDelta(1)
  pure val c1: Changes = genChange(1)
  pure val x1: Selection = selectTests(applyDelta(shop, d1), mainRules, c1)
  /// @conformance
  run generated1Run = init.then(buildWith(shop, d1, mainRules)).then(change(c1)).then(selectNow(x1))
    .then(all { assert(state.selection == x1), state' = state })

  pure val d2: Delta = genDelta(2)
  pure val c2: Changes = genChange(2)
  pure val x2: Selection = selectTests(applyDelta(shop, d2), mainRules, c2)
  /// @conformance
  run generated2Run = init.then(buildWith(shop, d2, mainRules)).then(change(c2)).then(selectNow(x2))
    .then(all { assert(state.selection == x2), state' = state })

  pure val d3: Delta = genDelta(3)
  pure val c3: Changes = genChange(3)
  pure val x3: Selection = selectTests(applyDelta(shop, d3), mainRules, c3)
  /// @conformance
  run generated3Run = init.then(buildWith(shop, d3, mainRules)).then(change(c3)).then(selectNow(x3))
    .then(all { assert(state.selection == x3), state' = state })

  pure val d4: Delta = genDelta(4)
  pure val c4: Changes = genChange(4)
  pure val x4: Selection = selectTests(applyDelta(shop, d4), mainRules, c4)
  /// @conformance
  run generated4Run = init.then(buildWith(shop, d4, mainRules)).then(change(c4)).then(selectNow(x4))
    .then(all { assert(state.selection == x4), state' = state })

  pure val d5: Delta = genDelta(5)
  pure val c5: Changes = genChange(5)
  pure val x5: Selection = selectTests(applyDelta(shop, d5), mainRules, c5)
  /// @conformance
  run generated5Run = init.then(buildWith(shop, d5, mainRules)).then(change(c5)).then(selectNow(x5))
    .then(all { assert(state.selection == x5), state' = state })

  pure val d6: Delta = genDelta(6)
  pure val c6: Changes = genChange(6)
  pure val x6: Selection = selectTests(applyDelta(shop, d6), mainRules, c6)
  /// @conformance
  run generated6Run = init.then(buildWith(shop, d6, mainRules)).then(change(c6)).then(selectNow(x6))
    .then(all { assert(state.selection == x6), state' = state })

  pure val d7: Delta = genDelta(7)
  pure val c7: Changes = genChange(7)
  pure val x7: Selection = selectTests(applyDelta(shop, d7), mainRules, c7)
  /// @conformance
  run generated7Run = init.then(buildWith(shop, d7, mainRules)).then(change(c7)).then(selectNow(x7))
    .then(all { assert(state.selection == x7), state' = state })

  pure val d8: Delta = genDelta(8)
  pure val c8: Changes = genChange(8)
  pure val x8: Selection = selectTests(applyDelta(shop, d8), mainRules, c8)
  /// @conformance
  run generated8Run = init.then(buildWith(shop, d8, mainRules)).then(change(c8)).then(selectNow(x8))
    .then(all { assert(state.selection == x8), state' = state })

  pure val d9: Delta = genDelta(9)
  pure val c9: Changes = genChange(9)
  pure val x9: Selection = selectTests(applyDelta(shop, d9), mainRules, c9)
  /// @conformance
  run generated9Run = init.then(buildWith(shop, d9, mainRules)).then(change(c9)).then(selectNow(x9))
    .then(all { assert(state.selection == x9), state' = state })

  pure val d10: Delta = genDelta(10)
  pure val c10: Changes = genChange(10)
  pure val x10: Selection = selectTests(applyDelta(shop, d10), mainRules, c10)
  /// @conformance
  run generated10Run = init.then(buildWith(shop, d10, mainRules)).then(change(c10)).then(selectNow(x10))
    .then(all { assert(state.selection == x10), state' = state })

  pure val d11: Delta = genDelta(11)
  pure val c11: Changes = genChange(11)
  pure val x11: Selection = selectTests(applyDelta(shop, d11), mainRules, c11)
  /// @conformance
  run generated11Run = init.then(buildWith(shop, d11, mainRules)).then(change(c11)).then(selectNow(x11))
    .then(all { assert(state.selection == x11), state' = state })

  pure val d12: Delta = genDelta(12)
  pure val c12: Changes = genChange(12)
  pure val x12: Selection = selectTests(applyDelta(shop, d12), mainRules, c12)
  /// @conformance
  run generated12Run = init.then(buildWith(shop, d12, mainRules)).then(change(c12)).then(selectNow(x12))
    .then(all { assert(state.selection == x12), state' = state })

  pure val d13: Delta = genDelta(13)
  pure val c13: Changes = genChange(13)
  pure val x13: Selection = selectTests(applyDelta(shop, d13), mainRules, c13)
  /// @conformance
  run generated13Run = init.then(buildWith(shop, d13, mainRules)).then(change(c13)).then(selectNow(x13))
    .then(all { assert(state.selection == x13), state' = state })

  pure val d14: Delta = genDelta(14)
  pure val c14: Changes = genChange(14)
  pure val x14: Selection = selectTests(applyDelta(shop, d14), mainRules, c14)
  /// @conformance
  run generated14Run = init.then(buildWith(shop, d14, mainRules)).then(change(c14)).then(selectNow(x14))
    .then(all { assert(state.selection == x14), state' = state })

  pure val d15: Delta = genDelta(15)
  pure val c15: Changes = genChange(15)
  pure val x15: Selection = selectTests(applyDelta(shop, d15), mainRules, c15)
  /// @conformance
  run generated15Run = init.then(buildWith(shop, d15, mainRules)).then(change(c15)).then(selectNow(x15))
    .then(all { assert(state.selection == x15), state' = state })

  pure val d16: Delta = genDelta(16)
  pure val c16: Changes = genChange(16)
  pure val x16: Selection = selectTests(applyDelta(shop, d16), mainRules, c16)
  /// @conformance
  run generated16Run = init.then(buildWith(shop, d16, mainRules)).then(change(c16)).then(selectNow(x16))
    .then(all { assert(state.selection == x16), state' = state })

  pure val d17: Delta = genDelta(17)
  pure val c17: Changes = genChange(17)
  pure val x17: Selection = selectTests(applyDelta(shop, d17), mainRules, c17)
  /// @conformance
  run generated17Run = init.then(buildWith(shop, d17, mainRules)).then(change(c17)).then(selectNow(x17))
    .then(all { assert(state.selection == x17), state' = state })

  pure val d18: Delta = genDelta(18)
  pure val c18: Changes = genChange(18)
  pure val x18: Selection = selectTests(applyDelta(shop, d18), mainRules, c18)
  /// @conformance
  run generated18Run = init.then(buildWith(shop, d18, mainRules)).then(change(c18)).then(selectNow(x18))
    .then(all { assert(state.selection == x18), state' = state })

  pure val d19: Delta = genDelta(19)
  pure val c19: Changes = genChange(19)
  pure val x19: Selection = selectTests(applyDelta(shop, d19), mainRules, c19)
  /// @conformance
  run generated19Run = init.then(buildWith(shop, d19, mainRules)).then(change(c19)).then(selectNow(x19))
    .then(all { assert(state.selection == x19), state' = state })

  pure val d20: Delta = genDelta(20)
  pure val c20: Changes = genChange(20)
  pure val x20: Selection = selectTests(applyDelta(shop, d20), mainRules, c20)
  /// @conformance
  run generated20Run = init.then(buildWith(shop, d20, mainRules)).then(change(c20)).then(selectNow(x20))
    .then(all { assert(state.selection == x20), state' = state })

  pure val d21: Delta = genDelta(21)
  pure val c21: Changes = genChange(21)
  pure val x21: Selection = selectTests(applyDelta(shop, d21), mainRules, c21)
  /// @conformance
  run generated21Run = init.then(buildWith(shop, d21, mainRules)).then(change(c21)).then(selectNow(x21))
    .then(all { assert(state.selection == x21), state' = state })

  pure val d22: Delta = genDelta(22)
  pure val c22: Changes = genChange(22)
  pure val x22: Selection = selectTests(applyDelta(shop, d22), mainRules, c22)
  /// @conformance
  run generated22Run = init.then(buildWith(shop, d22, mainRules)).then(change(c22)).then(selectNow(x22))
    .then(all { assert(state.selection == x22), state' = state })

  pure val d23: Delta = genDelta(23)
  pure val c23: Changes = genChange(23)
  pure val x23: Selection = selectTests(applyDelta(shop, d23), mainRules, c23)
  /// @conformance
  run generated23Run = init.then(buildWith(shop, d23, mainRules)).then(change(c23)).then(selectNow(x23))
    .then(all { assert(state.selection == x23), state' = state })

  pure val d24: Delta = genDelta(24)
  pure val c24: Changes = genChange(24)
  pure val x24: Selection = selectTests(applyDelta(shop, d24), mainRules, c24)
  /// @conformance
  run generated24Run = init.then(buildWith(shop, d24, mainRules)).then(change(c24)).then(selectNow(x24))
    .then(all { assert(state.selection == x24), state' = state })

  pure val d25: Delta = genDelta(25)
  pure val c25: Changes = genChange(25)
  pure val x25: Selection = selectTests(applyDelta(shop, d25), mainRules, c25)
  /// @conformance
  run generated25Run = init.then(buildWith(shop, d25, mainRules)).then(change(c25)).then(selectNow(x25))
    .then(all { assert(state.selection == x25), state' = state })

  pure val d26: Delta = genDelta(26)
  pure val c26: Changes = genChange(26)
  pure val x26: Selection = selectTests(applyDelta(shop, d26), mainRules, c26)
  /// @conformance
  run generated26Run = init.then(buildWith(shop, d26, mainRules)).then(change(c26)).then(selectNow(x26))
    .then(all { assert(state.selection == x26), state' = state })

  pure val d27: Delta = genDelta(27)
  pure val c27: Changes = genChange(27)
  pure val x27: Selection = selectTests(applyDelta(shop, d27), mainRules, c27)
  /// @conformance
  run generated27Run = init.then(buildWith(shop, d27, mainRules)).then(change(c27)).then(selectNow(x27))
    .then(all { assert(state.selection == x27), state' = state })

  pure val d28: Delta = genDelta(28)
  pure val c28: Changes = genChange(28)
  pure val x28: Selection = selectTests(applyDelta(shop, d28), mainRules, c28)
  /// @conformance
  run generated28Run = init.then(buildWith(shop, d28, mainRules)).then(change(c28)).then(selectNow(x28))
    .then(all { assert(state.selection == x28), state' = state })

  pure val d29: Delta = genDelta(29)
  pure val c29: Changes = genChange(29)
  pure val x29: Selection = selectTests(applyDelta(shop, d29), mainRules, c29)
  /// @conformance
  run generated29Run = init.then(buildWith(shop, d29, mainRules)).then(change(c29)).then(selectNow(x29))
    .then(all { assert(state.selection == x29), state' = state })

  pure val d30: Delta = genDelta(30)
  pure val c30: Changes = genChange(30)
  pure val x30: Selection = selectTests(applyDelta(shop, d30), mainRules, c30)
  /// @conformance
  run generated30Run = init.then(buildWith(shop, d30, mainRules)).then(change(c30)).then(selectNow(x30))
    .then(all { assert(state.selection == x30), state' = state })

  pure val d31: Delta = genDelta(31)
  pure val c31: Changes = genChange(31)
  pure val x31: Selection = selectTests(applyDelta(shop, d31), mainRules, c31)
  /// @conformance
  run generated31Run = init.then(buildWith(shop, d31, mainRules)).then(change(c31)).then(selectNow(x31))
    .then(all { assert(state.selection == x31), state' = state })

  pure val d32: Delta = genDelta(32)
  pure val c32: Changes = genChange(32)
  pure val x32: Selection = selectTests(applyDelta(shop, d32), mainRules, c32)
  /// @conformance
  run generated32Run = init.then(buildWith(shop, d32, mainRules)).then(change(c32)).then(selectNow(x32))
    .then(all { assert(state.selection == x32), state' = state })

  pure val d33: Delta = genDelta(33)
  pure val c33: Changes = genChange(33)
  pure val x33: Selection = selectTests(applyDelta(shop, d33), mainRules, c33)
  /// @conformance
  run generated33Run = init.then(buildWith(shop, d33, mainRules)).then(change(c33)).then(selectNow(x33))
    .then(all { assert(state.selection == x33), state' = state })

  pure val d34: Delta = genDelta(34)
  pure val c34: Changes = genChange(34)
  pure val x34: Selection = selectTests(applyDelta(shop, d34), mainRules, c34)
  /// @conformance
  run generated34Run = init.then(buildWith(shop, d34, mainRules)).then(change(c34)).then(selectNow(x34))
    .then(all { assert(state.selection == x34), state' = state })

  pure val d35: Delta = genDelta(35)
  pure val c35: Changes = genChange(35)
  pure val x35: Selection = selectTests(applyDelta(shop, d35), mainRules, c35)
  /// @conformance
  run generated35Run = init.then(buildWith(shop, d35, mainRules)).then(change(c35)).then(selectNow(x35))
    .then(all { assert(state.selection == x35), state' = state })

  pure val d36: Delta = genDelta(36)
  pure val c36: Changes = genChange(36)
  pure val x36: Selection = selectTests(applyDelta(shop, d36), mainRules, c36)
  /// @conformance
  run generated36Run = init.then(buildWith(shop, d36, mainRules)).then(change(c36)).then(selectNow(x36))
    .then(all { assert(state.selection == x36), state' = state })

  pure val d37: Delta = genDelta(37)
  pure val c37: Changes = genChange(37)
  pure val x37: Selection = selectTests(applyDelta(shop, d37), mainRules, c37)
  /// @conformance
  run generated37Run = init.then(buildWith(shop, d37, mainRules)).then(change(c37)).then(selectNow(x37))
    .then(all { assert(state.selection == x37), state' = state })

  pure val d38: Delta = genDelta(38)
  pure val c38: Changes = genChange(38)
  pure val x38: Selection = selectTests(applyDelta(shop, d38), mainRules, c38)
  /// @conformance
  run generated38Run = init.then(buildWith(shop, d38, mainRules)).then(change(c38)).then(selectNow(x38))
    .then(all { assert(state.selection == x38), state' = state })

  pure val d39: Delta = genDelta(39)
  pure val c39: Changes = genChange(39)
  pure val x39: Selection = selectTests(applyDelta(shop, d39), mainRules, c39)
  /// @conformance
  run generated39Run = init.then(buildWith(shop, d39, mainRules)).then(change(c39)).then(selectNow(x39))
    .then(all { assert(state.selection == x39), state' = state })

  pure val d40: Delta = genDelta(40)
  pure val c40: Changes = genChange(40)
  pure val x40: Selection = selectTests(applyDelta(shop, d40), mainRules, c40)
  /// @conformance
  run generated40Run = init.then(buildWith(shop, d40, mainRules)).then(change(c40)).then(selectNow(x40))
    .then(all { assert(state.selection == x40), state' = state })
}
