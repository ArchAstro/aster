#![allow(clippy::disallowed_macros)] // test diagnostics may use std print macros
//! End-to-end tests for `aster affected --explain`: why a project, a lane and
//! a test is selected, in two languages.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

struct Workspace {
    tmp: TempDir,
}

impl Workspace {
    fn new(config: &str, files: &[(&str, &str)]) -> Self {
        let workspace = Self {
            tmp: TempDir::new().unwrap(),
        };
        workspace.git(&["init", "-q", "-b", "main"]);
        workspace.git(&["config", "user.email", "test@test.com"]);
        workspace.git(&["config", "user.name", "Test"]);
        workspace.write("aster.toml", config);
        for (path, content) in files {
            workspace.write(path, content);
        }
        workspace.git(&["add", "-A"]);
        workspace.git(&["commit", "-q", "-m", "initial"]);
        workspace
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed: {output:?}");
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn edit(&self, path: &str, from: &str, to: &str) {
        let full = self.root().join(path);
        let content = fs::read_to_string(&full).unwrap();
        assert!(content.contains(from), "{path} does not contain {from:?}");
        fs::write(full, content.replacen(from, to, 1)).unwrap();
    }

    fn aster(&self, args: &[&str]) -> (String, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(self.root())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "aster {args:?}: {stdout}\n{stderr}"
        );
        (stdout, stderr)
    }

    fn json(&self, args: &[&str]) -> Value {
        let (stdout, _) = self.aster(args);
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}"))
    }
}

/// The target entry for `address` in a dry-run document.
fn target<'a>(plan: &'a Value, address: &str) -> &'a Value {
    plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["address"] == address)
        .unwrap_or_else(|| panic!("{address} is not in {plan:#}"))
}

/// The hops of a chain as `node <- edge` strings.
fn hops(chain: &Value) -> Vec<String> {
    chain
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| {
            format!(
                "{} <- {}",
                hop["node"].as_str().unwrap(),
                hop["edge"].as_str().unwrap()
            )
        })
        .collect()
}

const ELIXIR: &[(&str, &str)] = &[
    (
        "shop/mix.exs",
        "defmodule Shop.MixProject do\n  use Mix.Project\n  def project, do: [app: :shop, version: \"0.1.0\"]\nend\n",
    ),
    (
        "shop/lib/shop/pricing.ex",
        "defmodule Shop.Pricing do\n  def sum(items), do: Enum.sum(items)\nend\n",
    ),
    (
        "shop/lib/shop/cart.ex",
        "defmodule Shop.Cart do\n  alias Shop.Pricing\n\n  def total(cart), do: Pricing.sum(cart.items)\nend\n",
    ),
    (
        "shop/test/shop/cart_test.exs",
        "defmodule Shop.CartTest do\n  use ExUnit.Case\n  alias Shop.Cart\n\n  test \"total\" do\n    assert Cart.total(%{items: [1]}) == 1\n  end\nend\n",
    ),
];

#[test]
fn explain_gives_the_chain_from_the_change_to_each_selected_elixir_test() {
    let ws = Workspace::new("", ELIXIR);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );
    let plan = ws.json(&[
        "--json",
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--explain",
    ]);
    let explain = &target(&plan, "//shop:test")["explain"];
    assert_eq!(explain["project"], "//shop");
    assert_eq!(explain["selected_tests"], 1);
    let reasons = explain["reasons"].to_string();
    assert!(
        reasons.contains("owns 1 changed file: shop/lib/shop/pricing.ex"),
        "{reasons}"
    );
    assert!(
        reasons.contains("source graph reaches 1 test file"),
        "{reasons}"
    );

    let test = &explain["tests"][0];
    assert_eq!(test["file"], "shop/test/shop/cart_test.exs");
    assert_eq!(
        hops(&test["chain"]),
        [
            "sum <- shop/lib/shop/pricing.ex:2 changed",
            "total <- refers to `sum`",
            "total <- refers to `total`",
        ]
    );
}

#[test]
fn a_module_named_as_data_is_labelled_so_in_the_chain() {
    // A registry names a module in a map, and a lookup returns it. A change
    // to a callback nothing names reaches the registry as a user of the
    // module, and the chain says the registry only names it.
    let ws = Workspace::new(
        "",
        &[
            ("app/mix.exs", "defmodule App.MixProject do\n  use Mix.Project\n  def project, do: [app: :app, version: \"0.1.0\"]\nend\n"),
            (
                "app/lib/app/job.ex",
                "defmodule App.Job do\n  @behaviour App.Behaviour\n\n  @impl true\n  def run(arg), do: arg\nend\n",
            ),
            (
                "app/lib/app/registry.ex",
                "defmodule App.Registry do\n  @jobs %{\"job\" => App.Job}\n\n  def fetch(name), do: Map.fetch!(@jobs, name)\nend\n",
            ),
            (
                "app/lib/app/dispatcher.ex",
                "defmodule App.Dispatcher do\n  def dispatch(name, arg) do\n    mod = App.Registry.fetch(name)\n    mod.run(arg)\n  end\nend\n",
            ),
            (
                "app/test/app/dispatcher_test.exs",
                "defmodule App.DispatcherTest do\n  use ExUnit.Case\n\n  test \"dispatch\" do\n    assert App.Dispatcher.dispatch(\"job\", 1) == 1\n  end\nend\n",
            ),
        ],
    );
    ws.edit(
        "app/lib/app/job.ex",
        "def run(arg), do: arg",
        "def run(arg), do: arg + 0",
    );
    let plan = ws.json(&[
        "--json",
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--explain",
    ]);
    let test = &target(&plan, "//app:test")["explain"]["tests"][0];
    assert_eq!(test["file"], "app/test/app/dispatcher_test.exs");
    let edges = hops(&test["chain"]).join("\n");
    assert!(
        edges.contains("names the module as data (a list, map, struct or argument), not a call"),
        "{edges}"
    );
}

#[test]
fn explain_works_for_typescript_and_prints_text() {
    let ws = Workspace::new(
        "",
        &[
            ("money/package.json", r#"{"name":"@shop/money","scripts":{"test":"vitest run"}}"#),
            ("money/src/math.ts", "export function add(a: number, b: number) {\n  return a + b;\n}\n"),
            (
                "money/src/cart.ts",
                "import { add } from \"./math\";\n\nexport const total = (items: number[]) => items.reduce(add, 0);\n",
            ),
            (
                "money/src/cart.test.ts",
                "import { total } from \"./cart\";\nit(\"totals\", () => { expect(total([1])).toBe(1); });\n",
            ),
        ],
    );
    ws.edit("money/src/math.ts", "return a + b;", "return b + a;");

    let plan = ws.json(&[
        "--json",
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--explain",
    ]);
    let test = &target(&plan, "//money:test")["explain"]["tests"][0];
    assert_eq!(test["file"], "money/src/cart.test.ts");
    let edges = hops(&test["chain"]);
    assert_eq!(edges.first().unwrap(), "add <- money/src/math.ts:2 changed");
    assert!(
        edges.last().unwrap().ends_with("<- refers to `total`"),
        "{edges:?}"
    );

    let (text, _) = ws.aster(&["affected", "test", "--base=HEAD", "--related", "--explain"]);
    assert!(text.contains("why selected:"), "{text}");
    assert!(
        text.contains("money/src/cart.test.ts (1 test(s), 2 hops)"),
        "{text}"
    );
    assert!(
        text.contains("0. add (money/src/math.ts:1)  changed: money/src/math.ts:2 changed"),
        "{text}"
    );
}

#[test]
fn matrix_rows_say_why_the_lane_runs_and_nothing_changes_without_the_flag() {
    let config = "[affected.lanes.elixir]\ninclude = [\"//shop\"]\nname = \"Elixir\"\n\n[affected.lanes.other]\ninclude = [\"//other\"]\n";
    let mut files = ELIXIR.to_vec();
    files.push(("other/mix.exs", "defmodule Other.MixProject do\n  use Mix.Project\n  def project, do: [app: :other, version: \"0.1.0\"]\nend\n"));
    let ws = Workspace::new(config, &files);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );

    let rows = ws.json(&[
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--matrix",
        "--explain",
    ]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["lane"], "elixir");
    assert_eq!(rows[0]["explain"][0]["project"], "//shop");
    assert_eq!(rows[0]["explain"][0]["selected_tests"], 1);

    let plain = ws.json(&["affected", "test", "--base=HEAD", "--related", "--matrix"]);
    assert!(plain[0].get("explain").is_none(), "{plain}");
    let plan = ws.json(&[
        "--json",
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--dry-run",
    ]);
    assert!(
        target(&plan, "//shop:test").get("explain").is_none(),
        "{plan}"
    );
}

#[test]
fn explain_limit_caps_the_listed_tests_and_says_how_many_are_left() {
    let mut files = ELIXIR.to_vec();
    files.push((
        "shop/test/shop/second_test.exs",
        "defmodule Shop.SecondTest do\n  use ExUnit.Case\n\n  test \"sum\" do\n    assert Shop.Pricing.sum([1]) == 1\n  end\nend\n",
    ));
    let ws = Workspace::new("", &files);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );
    let plan = ws.json(&[
        "--json",
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--explain",
        "--explain-limit",
        "1",
    ]);
    let explain = &target(&plan, "//shop:test")["explain"];
    assert_eq!(explain["selected_tests"], 2);
    assert_eq!(explain["tests"].as_array().unwrap().len(), 1);
    // The shorter chain comes first.
    assert_eq!(
        explain["tests"][0]["file"],
        "shop/test/shop/second_test.exs"
    );

    let (text, _) = ws.aster(&[
        "affected",
        "test",
        "--base=HEAD",
        "--related",
        "--explain",
        "--explain-limit",
        "1",
    ]);
    assert!(text.contains("and 1 more selected test files"), "{text}");
}

#[test]
fn explain_without_related_names_the_rule_that_selects_each_project() {
    let ws = Workspace::new(
        "",
        &[
            (
                "lib/package.json",
                r#"{"name":"lib","scripts":{"test":"true"}}"#,
            ),
            (
                "app/package.json",
                r#"{"name":"app","dependencies":{"lib":"file:../lib"},"scripts":{"test":"true"}}"#,
            ),
            ("lib/a.txt", "a\n"),
        ],
    );
    ws.edit("lib/a.txt", "a", "aa");
    let plan = ws.json(&[
        "--json",
        "affected",
        "test",
        "--base=HEAD",
        "--dependents",
        "--explain",
    ]);
    let lib = target(&plan, "//lib:test")["explain"]["reasons"].to_string();
    assert!(lib.contains("owns 1 changed file: lib/a.txt"), "{lib}");
    let app = target(&plan, "//app:test")["explain"]["reasons"].to_string();
    assert!(
        app.contains("depends on a project that owns a changed file: //app -> //lib"),
        "{app}"
    );
}
