//! Each test pins one rule or invariant on the smallest repo that shows it. Some
//! are divergences the model found that main has since fixed (D1, D5), written
//! now as the behaviour that holds; the rest assert what the engine still does.
//! When one fails, the engine changed and the model's rule switch needs updating.
//!
//! The same cases are written against the model in `witnesses.qnt`.

use aster_related_model::engine::{Plan, Workspace};

const MIX: (&str, &str) = (
    "shop/mix.exs",
    "defmodule Shop.MixProject do\n  use Mix.Project\n  def project, do: [app: :shop, version: \"0.1.0\"]\nend\n",
);

fn narrow(plan: &Plan) -> Vec<&str> {
    assert!(!plan.full, "ran in full: {}", plan.notes);
    plan.files.iter().map(String::as_str).collect()
}

/// D1 (fixed by #354). Adding an edit never selects fewer tests.
///
/// `helper` is called by `Dead.run`, which nothing uses, so editing it alone
/// reaches no test and selects nothing. Editing `used` as well reaches the test
/// that calls it. Before #354 the first change ran the whole project, which was
/// more than the second.
#[test]
fn a_larger_change_selects_at_least_what_a_smaller_one_does() {
    // Setup: one file with two functions; the first is reachable only from
    // dead code, the second from a test. A second test touches neither.
    let files = [
        MIX,
        (
            "shop/lib/pipeline.ex",
            "defmodule Shop.Pipeline do\n  def helper(x), do: x\n  def used(x), do: x\nend\n",
        ),
        (
            "shop/lib/dead.ex",
            "defmodule Shop.Dead do\n  def run(x), do: Shop.Pipeline.helper(x)\nend\n",
        ),
        (
            "shop/test/pipeline_test.exs",
            "defmodule Shop.PipelineTest do\n  use ExUnit.Case\n\n  test \"used\" do\n    assert Shop.Pipeline.used(1) == 1\n  end\nend\n",
        ),
        (
            "shop/test/other_test.exs",
            "defmodule Shop.OtherTest do\n  use ExUnit.Case\n\n  test \"other\" do\n    assert 1 == 1\n  end\nend\n",
        ),
    ];

    // Action: edit `helper` alone.
    let ws = Workspace::from_files(&files).unwrap();
    ws.edit(
        "shop/lib/pipeline.ex",
        "helper(x), do: x",
        "helper(x), do: x + 0",
    )
    .unwrap();
    // Outcome: nothing reaches a test, so nothing runs.
    let small = ws.plan().unwrap();
    assert_eq!(small, Plan::default(), "{small:?}");

    // Action: edit `used` too.
    ws.edit(
        "shop/lib/pipeline.ex",
        "used(x), do: x",
        "used(x), do: x + 0",
    )
    .unwrap();
    // Outcome: the larger change selects the test that calls `used`.
    let big = ws.plan().unwrap();
    assert_eq!(narrow(&big), ["test/pipeline_test.exs"]);
}

/// D4. A change to a trailing comment selects tests.
///
/// The index classifies a line as comment only when the whole line is one.
#[test]
fn editing_only_a_trailing_comment_selects_tests() {
    let files = [
        MIX,
        (
            "shop/lib/a.ex",
            "defmodule Shop.A do\n  # Adds one.\n  def f(x) do\n    x + 1 # note\n  end\nend\n",
        ),
        (
            "shop/test/a_test.exs",
            "defmodule Shop.ATest do\n  use ExUnit.Case\n\n  test \"f\" do\n    assert Shop.A.f(1) == 2\n  end\nend\n",
        ),
    ];
    let ws = Workspace::from_files(&files).unwrap();

    // A comment on its own line selects nothing.
    ws.edit("shop/lib/a.ex", "# Adds one.", "# Adds 1.")
        .unwrap();
    assert_eq!(ws.plan().unwrap(), Plan::default());

    // A comment after code is part of the line that holds the code.
    ws.edit("shop/lib/a.ex", "# Adds 1.", "# Adds one.")
        .unwrap();
    ws.edit("shop/lib/a.ex", "x + 1 # note", "x + 1 # remark")
        .unwrap();
    assert_eq!(narrow(&ws.plan().unwrap()), ["test/a_test.exs"]);
}

/// D2a. Top-level code fans out to every user of the module.
///
/// The test calls `other/0`, which does not read the attribute.
#[test]
fn editing_top_level_code_selects_users_of_functions_that_do_not_read_it() {
    let files = [
        MIX,
        (
            "shop/lib/limits.ex",
            "defmodule Shop.Limits do\n  @limit 5\n\n  def limit, do: @limit\n  def other, do: :ok\nend\n",
        ),
        (
            "shop/test/limits_test.exs",
            "defmodule Shop.LimitsTest do\n  use ExUnit.Case\n\n  test \"other\" do\n    assert Shop.Limits.other() == :ok\n  end\nend\n",
        ),
    ];
    let ws = Workspace::from_files(&files).unwrap();
    ws.edit("shop/lib/limits.ex", "@limit 5", "@limit 6")
        .unwrap();
    assert_eq!(narrow(&ws.plan().unwrap()), ["test/limits_test.exs"]);
}

const ROUTER_REPO: [(&str, &str); 5] = [
    MIX,
    (
        "shop/lib/router.ex",
        "defmodule Shop.Router do\n  get \"/orders/:id\", Shop.OrderController, :show\n\n  def helper, do: :ok\nend\n",
    ),
    (
        "shop/lib/order_controller.ex",
        "defmodule Shop.OrderController do\n  def show(conn), do: conn\nend\n",
    ),
    (
        "shop/test/router_test.exs",
        "defmodule Shop.RouterTest do\n  use ExUnit.Case\n\n  test \"helper\" do\n    assert Shop.Router.helper() == :ok\n  end\nend\n",
    ),
    (
        "shop/test/other_test.exs",
        "defmodule Shop.OtherTest do\n  use ExUnit.Case\n\n  test \"other\" do\n    assert 1 == 1\n  end\nend\n",
    ),
];

/// D2b, D3 (fixed by #349). A route no code requests reaches nothing further.
///
/// Before ArchAstro/aster#349 it fanned out to every user of its router, here
/// `router_test`. The change now reaches no test and selects nothing.
#[test]
fn a_route_nothing_requests_selects_nothing() {
    let ws = Workspace::from_files(&ROUTER_REPO).unwrap();
    ws.edit("shop/lib/order_controller.ex", "do: conn", "do: {conn, 1}")
        .unwrap();
    let plan = ws.plan().unwrap();
    assert_eq!(plan, Plan::default(), "{plan:?}");
}

/// D5 (fixed by #354). A reflecting test counts as reaching a change.
///
/// The test that lists the application's modules is reached by any change to
/// non-test code; before #354 `unused` still "reached no test" and the project
/// ran in full instead of that one test.
#[test]
fn a_change_only_a_reflecting_test_reaches_selects_that_test() {
    let files = [
        MIX,
        (
            "shop/lib/a.ex",
            "defmodule Shop.A do\n  def unused(x), do: x\nend\n",
        ),
        (
            "shop/test/modules_test.exs",
            "defmodule Shop.ModulesTest do\n  use ExUnit.Case\n\n  test \"modules\" do\n    {:ok, modules} = :application.get_key(:shop, :modules)\n    assert is_list(modules)\n  end\nend\n",
        ),
        (
            "shop/test/other_test.exs",
            "defmodule Shop.OtherTest do\n  use ExUnit.Case\n\n  test \"other\" do\n    assert 1 == 1\n  end\nend\n",
        ),
    ];
    let ws = Workspace::from_files(&files).unwrap();
    ws.edit("shop/lib/a.ex", "do: x", "do: x + 0").unwrap();
    assert_eq!(narrow(&ws.plan().unwrap()), ["test/modules_test.exs"]);
}

/// D6. A computed module name is found only when the same definition also
/// names the module some other way.
#[test]
fn a_function_called_by_computed_module_name_is_found_only_beside_a_mention_of_its_module() {
    let call = |extra: &str| {
        format!(
            "defmodule Shop.{{}}Test do\n  use ExUnit.Case\n\n  test \"t\" do\n    apply(Module.concat([\"Shop\", \"A\"]), :f, [1])\n{extra}  end\nend\n"
        )
    };
    let files = [
        MIX,
        (
            "shop/lib/a.ex",
            "defmodule Shop.A do\n  def f(x), do: x\n  def g(x), do: x\nend\n",
        ),
        ("shop/test/blind_test.exs", &call("").replace("{}", "Blind")),
        (
            "shop/test/seen_test.exs",
            &call("    Shop.A.g(1)\n").replace("{}", "Seen"),
        ),
    ];
    let ws = Workspace::from_files(&files).unwrap();
    ws.edit("shop/lib/a.ex", "def f(x), do: x", "def f(x), do: x + 0")
        .unwrap();
    assert_eq!(narrow(&ws.plan().unwrap()), ["test/seen_test.exs"]);
}

/// D7 (fixed by #354). A message clause nobody sends selects nothing.
///
/// `handle_cast({:ping}, ..)` is reached by what may send `:ping`. The test only
/// starts the worker, so nothing reaches it; before #354 that ran the project.
#[test]
fn a_message_clause_nobody_sends_selects_nothing() {
    let files = [
        MIX,
        (
            "shop/lib/worker.ex",
            "defmodule Shop.Worker do\n  def start_link(arg), do: arg\n\n  def handle_cast({:ping}, state), do: state\nend\n",
        ),
        (
            "shop/test/worker_test.exs",
            "defmodule Shop.WorkerTest do\n  use ExUnit.Case\n\n  test \"starts\" do\n    assert Shop.Worker.start_link(1) == 1\n  end\nend\n",
        ),
    ];
    let ws = Workspace::from_files(&files).unwrap();
    ws.edit(
        "shop/lib/worker.ex",
        "state), do: state",
        "state), do: {state, 1}",
    )
    .unwrap();
    assert_eq!(ws.plan().unwrap(), Plan::default());
}
