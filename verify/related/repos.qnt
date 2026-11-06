// Repos the scenarios run the engine on. Every definition id is unique;
// units 1-9 are lib files, units 101 and up are test files.
module repos {
  import related.* from "./model"

  type Row = { id: int, unit: int, kind: str, name: str }

  pure def withDefs(base: Repo, rows: Set[Row]): Repo =
    base
      .with("kindOf", rows.map(x => x.id).mapBy(i => rows.fold("", (a, x) => if (x.id == i) x.kind else a)))
      .with("unitOf", rows.map(x => x.id).mapBy(i => rows.fold(0, (a, x) => if (x.id == i) x.unit else a)))
      .with("nameOf", rows.map(x => x.id).mapBy(i => rows.fold("", (a, x) => if (x.id == i) x.name else a)))

  // A shop: pricing and cart, a worker, a macro module, a schema pair, a router
  // with a controller, and one test for each way a test can reach code.
  //
  //   unit 1 Pricing     10 sum      11 tax      12 unused (nothing names it)  13 top
  //   unit 2 Cart        20 total -> 10, 21 count, 22 blank (%Order{})
  //   unit 3 Worker      30 init  31 handle_call  32 start_link   (callbacks)
  //   unit 4 Tracing     40 traced (macro)
  //   unit 5 Order       50 changeset  51 top (belongs_to Customer)
  //   unit 6 Customer    60 title  61 top  62 age  63 adult -> 62
  //   unit 7 Router      70 route /s1/:id -> 80   71 route /s2/:id -> 81   72 helper
  //   unit 8 Controller  80 show   81 index
  //   unit 9 Pipeline    90 helper (named by 94)   91 used (named by t1100)
  //   unit 12 Dead       94 run -> 90 (nothing names it, nothing uses unit 12)
  //   unit 5 also holds  52 legacy (nothing names it)
  pure val shopDefs: Set[Row] = Set(
    { id: 10, unit: 1, kind: "fn", name: "sum" },
    { id: 11, unit: 1, kind: "fn", name: "tax" },
    { id: 12, unit: 1, kind: "fn", name: "unused" },
    { id: 13, unit: 1, kind: "top", name: "top" },
    { id: 20, unit: 2, kind: "fn", name: "total" },
    { id: 21, unit: 2, kind: "fn", name: "count" },
    { id: 22, unit: 2, kind: "fn", name: "blank" },
    { id: 30, unit: 3, kind: "callback", name: "init" },
    { id: 31, unit: 3, kind: "callback", name: "handle_call" },
    { id: 32, unit: 3, kind: "callback", name: "start_link" },
    { id: 40, unit: 4, kind: "macro", name: "traced" },
    { id: 50, unit: 5, kind: "fn", name: "changeset" },
    { id: 51, unit: 5, kind: "top", name: "top" },
    { id: 60, unit: 6, kind: "fn", name: "title" },
    { id: 61, unit: 6, kind: "top", name: "top" },
    { id: 62, unit: 6, kind: "fn", name: "age" },
    { id: 63, unit: 6, kind: "fn", name: "adult" },
    { id: 70, unit: 7, kind: "route", name: "route70" },
    { id: 71, unit: 7, kind: "route", name: "route71" },
    { id: 72, unit: 7, kind: "fn", name: "helper" },
    { id: 80, unit: 8, kind: "fn", name: "show" },
    { id: 81, unit: 8, kind: "fn", name: "index" },
    { id: 52, unit: 5, kind: "fn", name: "legacy" },
    { id: 90, unit: 9, kind: "fn", name: "helper" },
    { id: 91, unit: 9, kind: "fn", name: "used" },
    { id: 94, unit: 12, kind: "fn", name: "run" },
    // Tests, one file each.
    { id: 1010, unit: 101, kind: "test", name: "t1010" },   // Cart.total, Pricing.tax
    { id: 1020, unit: 102, kind: "test", name: "t1020" },   // Worker.start_link
    { id: 1030, unit: 103, kind: "test", name: "t1030" },   // GET /s1/#{id}
    { id: 1040, unit: 104, kind: "test", name: "t1040" },   // use Tracing
    { id: 1050, unit: 105, kind: "test", name: "t1050" },   // lists modules at run time (shop only)
    { id: 1060, unit: 106, kind: "test", name: "t1060" },   // Cart.count
    { id: 1070, unit: 107, kind: "test", name: "t1070" },   // preload, a built path
    { id: 1080, unit: 108, kind: "test", name: "t1080" },   // Order.changeset
    { id: 1090, unit: 109, kind: "test", name: "t1090" },   // Router.helper
    { id: 1100, unit: 110, kind: "test", name: "t1100" }    // Orphans.used
  )

  pure val shop: Repo =
    withDefs(emptyRepo, shopDefs)
      .with("testUnits", Set(101, 102, 103, 104, 105, 106, 107, 108, 109, 110))
      .with("calls", Set(
        { from: 20, to: 10 },
        { from: 94, to: 90 },
        { from: 1010, to: 20 }, { from: 1010, to: 11 },
        { from: 1020, to: 32 },
        { from: 1060, to: 21 },
        { from: 1080, to: 50 },
        { from: 1090, to: 72 },
        { from: 1100, to: 91 }))
      .with("bare", Set({ from: 63, to: 62 }))
      .with("imports", Set({ from: 104, to: 4 }))
      .with("shapes", Set({ from: 22, to: 5 }, { from: 51, to: 6 }))
      .with("routes", Map(70 -> List(1, 0), 71 -> List(2, 0)))
      .with("serves", Map(70 -> 80, 71 -> 81))
      .with("requests", Set({ id: 1030, path: List(1, 0) }))
      .with("reflects", Set(1050))
      .with("preloads", Set({ from: 1070, to: 6 }))
      .with("builtPaths", Set({ from: 1070, to: 70 }))
      .with("computed", Set({ from: 1070, to: 91 }))

  // A worker with message clauses (tags 5 and 6) and three tests that use it: one
  // sends tag 5, one sends nothing, one sends a message it computes.
  pure val workerDefs: Set[Row] = Set(
    { id: 31, unit: 3, kind: "callback", name: "handle_call" },
    { id: 32, unit: 3, kind: "callback", name: "start_link" },
    { id: 33, unit: 3, kind: "callback", name: "handle_cast" },
    { id: 1020, unit: 102, kind: "test", name: "t1020" },
    { id: 1110, unit: 111, kind: "test", name: "t1110" },
    { id: 1120, unit: 112, kind: "test", name: "t1120" },
    { id: 1130, unit: 113, kind: "test", name: "t1130" }
  )

  pure val workers: Repo =
    withDefs(emptyRepo, workerDefs)
      .with("testUnits", Set(102, 111, 112, 113))
      .with("calls", Set({ from: 1020, to: 32 }, { from: 1110, to: 32 }, { from: 1120, to: 32 }))
      .with("shapes", Set({ from: 1130, to: 3 }))
      .with("accepts", Map(31 -> 5, 33 -> 6))
      .with("sends", Set({ id: 1020, tag: 5 }))
      .with("sendsAny", Set(1120))

  // The same repo without the test that lists modules, which every change reaches.
  pure val shopQuiet: Repo = shop.with("reflects", Set())
}
