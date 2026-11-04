//! Each test pins one place where the engine departs from a stated rule or
//! invariant, on the smallest repo that shows it. They assert what the engine
//! does today: when one fails, the engine changed, and the divergence list in
//! the pull request that added this file needs updating.
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

/// D1. Adding an edit can select fewer tests.
///
/// `helper` is called by `Dead.run`, which nothing uses, so editing it alone
/// "reaches no test" and the whole project runs. Editing `used` as well, in
/// the same file, reaches the test that calls it, and the project then runs
/// only that test.
#[test]
fn a_larger_change_selects_fewer_tests_than_a_smaller_one() {
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
    let small = ws.plan().unwrap();
    // Outcome: nothing reached a test, so every test runs.
    assert!(small.full, "{small:?}");
    assert!(small.notes.contains("reaches no test"), "{}", small.notes);

    // Action: edit `used` too.
    ws.edit(
        "shop/lib/pipeline.ex",
        "used(x), do: x",
        "used(x), do: x + 0",
    )
    .unwrap();
    let big = ws.plan().unwrap();
    // Outcome: the larger change runs one test; the smaller one ran two.
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

/// D2b. A route no code requests fans out to every user of its router (main).
///
/// ArchAstro/aster#349 removes this; see D3 for what replaces it.
#[test]
fn a_route_nothing_requests_selects_users_of_the_router_module() {
    let ws = Workspace::from_files(&ROUTER_REPO).unwrap();
    ws.edit("shop/lib/order_controller.ex", "do: conn", "do: {conn, 1}")
        .unwrap();
    assert_eq!(narrow(&ws.plan().unwrap()), ["test/router_test.exs"]);
}

/// D5. Reflection counts for the selection but not for the fallback.
///
/// The test that lists the application's modules is reached by any change to
/// non-test code, yet `unused` still "reaches no test", so the project runs in
/// full instead of running that one test.
#[test]
fn a_change_only_a_reflecting_test_reaches_still_runs_in_full() {
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
    let plan = ws.plan().unwrap();
    assert!(plan.full, "{plan:?}");
    assert!(plan.notes.contains("reaches no test"), "{}", plan.notes);
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
