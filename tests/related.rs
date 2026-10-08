//! End-to-end tests for `aster affected --related`: source-level selection
//! of the tests a change reaches.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

struct Workspace {
    tmp: TempDir,
}

impl Workspace {
    /// A committed git workspace holding `files`.
    fn new(files: &[(&str, &str)]) -> Self {
        let workspace = Self {
            tmp: TempDir::new().unwrap(),
        };
        workspace.git(&["init", "-q", "-b", "main"]);
        workspace.git(&["config", "user.email", "test@test.com"]);
        workspace.git(&["config", "user.name", "Test"]);
        workspace.write("aster.toml", "");
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

    /// Replace `from` with `to` in `path`.
    fn edit(&self, path: &str, from: &str, to: &str) {
        let full = self.root().join(path);
        let content = fs::read_to_string(&full).unwrap();
        assert!(content.contains(from), "{path} does not contain {from:?}");
        fs::write(full, content.replacen(from, to, 1)).unwrap();
    }

    fn aster(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(self.root())
            .output()
            .unwrap()
    }

    /// The dry-run plan of `aster affected <target> --related`.
    fn plan(&self, target: &str) -> Plan {
        let output = self.aster(&[
            "--json",
            "affected",
            target,
            "--base=HEAD",
            "--related",
            "--dry-run",
        ]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "aster failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("invalid JSON ({error}): {stdout}"));
        let mut plan = Plan::default();
        for entry in json["targets"].as_array().into_iter().flatten() {
            let address = entry["address"].as_str().unwrap().to_string();
            if !address.ends_with(&format!(":{target}")) {
                continue;
            }
            let strings = |key: &str| -> Vec<String> {
                entry[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect()
            };
            plan.targets
                .insert(address, (strings("commands"), strings("selection")));
        }
        plan.skipped = json["skipped"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        plan
    }
}

/// Requested targets that run, with their replacement commands and the
/// reasoning, and the targets dropped.
#[derive(Debug, Default)]
struct Plan {
    targets: BTreeMap<String, (Vec<String>, Vec<String>)>,
    skipped: Vec<String>,
}

impl Plan {
    fn commands(&self, address: &str) -> &[String] {
        match self.targets.get(address) {
            Some((commands, _)) => commands,
            None => panic!("{address} is not in the plan: {self:#?}"),
        }
    }

    fn notes(&self, address: &str) -> String {
        match self.targets.get(address) {
            Some((_, notes)) => notes.join("\n"),
            None => panic!("{address} is not in the plan: {self:#?}"),
        }
    }

    fn runs(&self) -> Vec<&str> {
        self.targets.keys().map(String::as_str).collect()
    }
}

const ELIXIR: &[(&str, &str)] = &[
    (
        "shop/mix.exs",
        "defmodule Shop.MixProject do\n  use Mix.Project\n  def project, do: [app: :shop, version: \"0.1.0\"]\nend\n",
    ),
    (
        "shop/lib/shop/pricing.ex",
        "defmodule Shop.Pricing do\n  # Sums the items.\n  def sum(items), do: Enum.sum(items)\n\n  def tax(amount), do: amount * 0.2\nend\n",
    ),
    (
        "shop/lib/shop/cart.ex",
        "defmodule Shop.Cart do\n  alias Shop.Pricing\n\n  def total(cart), do: Pricing.sum(cart.items)\n\n  def count(cart), do: length(cart.items)\nend\n",
    ),
    (
        "shop/lib/shop/worker.ex",
        "defmodule Shop.Worker do\n  use GenServer\n\n  def start_link(arg), do: GenServer.start_link(__MODULE__, arg)\n\n  @impl true\n  def handle_call(:ping, _from, state), do: {:reply, :pong, state}\nend\n",
    ),
    (
        "shop/test/shop/cart_test.exs",
        "defmodule Shop.CartTest do\n  use ExUnit.Case\n  alias Shop.Cart\n\n  test \"total\" do\n    assert Cart.total(%{items: [1]}) == 1\n  end\nend\n",
    ),
    (
        "shop/test/shop/tax_test.exs",
        "defmodule Shop.TaxTest do\n  use ExUnit.Case\n\n  test \"tax\" do\n    assert Shop.Pricing.tax(10) == 2.0\n  end\nend\n",
    ),
    (
        "shop/test/shop/worker_test.exs",
        "defmodule Shop.WorkerTest do\n  use ExUnit.Case\n\n  test \"ping\" do\n    {:ok, pid} = Shop.Worker.start_link([])\n    assert GenServer.call(pid, :ping) == :pong\n  end\nend\n",
    ),
];

#[test]
fn elixir_change_selects_the_tests_that_reach_the_function() {
    let ws = Workspace::new(ELIXIR);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/cart_test.exs"]
    );
    let notes = plan.notes("//shop:test");
    assert!(
        notes.contains("total (shop/lib/shop/cart.ex:4) ← sum (shop/lib/shop/pricing.ex:3)"),
        "{notes}"
    );
}

#[test]
fn comment_only_change_runs_no_tests() {
    let ws = Workspace::new(ELIXIR);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "# Sums the items.",
        "# Adds them up.",
    );
    let plan = ws.plan("test");
    assert_eq!(plan.skipped, ["//shop:test"]);
    assert!(plan.runs().is_empty(), "{plan:#?}");
}

#[test]
fn alias_change_selects_only_the_functions_that_use_the_module() {
    let ws = Workspace::new(ELIXIR);
    // Reordering an alias changes its line without changing any function.
    ws.edit(
        "shop/lib/shop/cart.ex",
        "  alias Shop.Pricing\n",
        "  alias Shop.Pricing, as: Pricing\n",
    );
    let plan = ws.plan("test");
    // `total` uses Pricing; `count` does not, and nothing else tests Cart.
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/cart_test.exs"]
    );
    let notes = plan.notes("//shop:test");
    assert!(notes.contains("total (shop/lib/shop/cart.ex:4)"), "{notes}");
}

#[test]
fn framework_callback_change_selects_users_of_the_module() {
    let ws = Workspace::new(ELIXIR);
    ws.edit("shop/lib/shop/worker.ex", ":pong, state", ":pang, state");
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/worker_test.exs"]
    );
}

#[test]
fn a_test_that_lists_modules_at_run_time_runs_for_any_module_change() {
    let mut files = ELIXIR.to_vec();
    // Reaches every module's functions without naming one of them.
    files.push((
        "shop/test/shop/modules_test.exs",
        "defmodule Shop.ModulesTest do\n  use ExUnit.Case\n\n  test \"every module answers\" do\n    {:ok, modules} = :application.get_key(:shop, :modules)\n    for module <- modules, do: assert(module.module_info(:module) == module)\n  end\nend\n",
    ));
    let ws = Workspace::new(&files);
    ws.edit("shop/lib/shop/pricing.ex", "amount * 0.2", "amount * 0.25");
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/modules_test.exs test/shop/tax_test.exs"]
    );
    let notes = plan.notes("//shop:test");
    assert!(notes.contains("lists modules at run time"), "{notes}");
}

#[test]
fn graph_source_prints_what_a_change_reaches() {
    let ws = Workspace::new(ELIXIR);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );
    let text = |args: &[&str]| {
        let output = ws.aster(args);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };

    // No range: the working tree.
    let working = text(&["graph", "--source"]);
    assert_eq!(
        working,
        "Source graph for the working tree: 1 changed, 2 reached, 1 in test files\n\
         \nsum (shop/lib/shop/pricing.ex:3)  [shop/lib/shop/pricing.ex:3 changed]\n\
         └─ total (shop/lib/shop/cart.ex:4)\n   \
         └─ total (shop/test/shop/cart_test.exs:5)  [test]\n"
    );

    ws.git(&["commit", "-qam", "change"]);
    assert!(text(&["graph", "--source"]).contains("0 changed, 0 reached"));
    // A range, a three-dot range and a single ref all name the commit.
    for range in ["HEAD~1..HEAD", "HEAD~1...HEAD", "HEAD~1"] {
        let committed = text(&["graph", "--source", "--commit", range]);
        assert!(
            committed.contains("└─ total (shop/lib/shop/cart.ex:4)"),
            "{range}: {committed}"
        );
    }
    // Filters drop changed files before the graph is built.
    let filtered = text(&[
        "graph",
        "--source",
        "--commit",
        "HEAD~1..HEAD",
        "--ext",
        "ts",
    ]);
    assert!(filtered.contains("0 changed"), "{filtered}");
    let scoped = text(&[
        "graph",
        "--source",
        "--commit",
        "HEAD~1..HEAD",
        "--dir",
        "shop/lib",
    ]);
    assert!(scoped.contains("1 changed"), "{scoped}");

    let json: serde_json::Value = serde_json::from_str(&text(&[
        "--json",
        "graph",
        "--source",
        "--commit",
        "HEAD~1..HEAD",
    ]))
    .unwrap();
    assert_eq!(json["range"], "HEAD~1..HEAD");
    assert_eq!(json["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(json["edges"].as_array().unwrap().len(), 2);
}

#[test]
fn removed_function_selects_the_tests_that_still_name_it() {
    let ws = Workspace::new(ELIXIR);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "\n  def tax(amount), do: amount * 0.2\n",
        "",
    );
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/tax_test.exs"]
    );
}

#[test]
fn manifest_change_runs_the_project_in_full() {
    let ws = Workspace::new(ELIXIR);
    ws.edit("shop/mix.exs", "0.1.0", "0.2.0");
    let plan = ws.plan("test");
    assert!(plan.commands("//shop:test").is_empty());
    assert!(plan
        .notes("//shop:test")
        .contains("shop/mix.exs changed; running in full"));
}

#[test]
fn data_files_select_the_tests_that_name_them_and_prose_selects_nothing() {
    let mut files = ELIXIR.to_vec();
    files.push(("shop/priv/prices.json", "{}\n"));
    files.push(("shop/priv/unused.bin", "x\n"));
    files.push(("shop/NOTES.md", "notes\n"));
    files.push((
        "shop/test/shop/prices_test.exs",
        "defmodule Shop.PricesTest do\n  use ExUnit.Case\n\n  test \"prices\" do\n    assert File.read!(\"priv/prices.json\") == \"{}\\n\"\n  end\nend\n",
    ));
    let ws = Workspace::new(&files);
    ws.write("shop/priv/prices.json", "{\"a\": 1}\n");
    ws.write("shop/NOTES.md", "more notes\n");
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/prices_test.exs"]
    );

    // A file nothing names could be read by anything.
    ws.write("shop/priv/unused.bin", "y\n");
    let plan = ws.plan("test");
    assert!(plan.commands("//shop:test").is_empty());
    assert!(plan
        .notes("//shop:test")
        .contains("shop/priv/unused.bin is not named by any source file"));
}

const TYPESCRIPT: &[(&str, &str)] = &[
    (
        "money/package.json",
        r#"{"name":"@shop/money","main":"dist/index.js","scripts":{"test":"vitest run","build":"tsc"}}"#,
    ),
    ("money/src/index.ts", "export * from \"./math\";\nexport * from \"./format\";\n"),
    (
        "money/src/math.ts",
        "export function add(a: number, b: number) {\n  return a + b;\n}\n",
    ),
    (
        "money/src/format.ts",
        "export function format(n: number) {\n  return `$${n}`;\n}\n",
    ),
    (
        "money/src/math.test.ts",
        "import { add } from \"./math\";\nit(\"adds\", () => { expect(add(1, 2)).toBe(3); });\n",
    ),
    (
        "money/src/format.test.ts",
        "import { format } from \"./format\";\nit(\"formats\", () => { expect(format(1)).toBe(\"$1\"); });\n",
    ),
    (
        "cart/package.json",
        r#"{"name":"@shop/cart","dependencies":{"@shop/money":"file:../money"},"scripts":{"test":"vitest run --passWithNoTests","build":"tsc"}}"#,
    ),
    (
        "cart/src/cart.ts",
        "import { add } from \"@shop/money\";\n\nexport const total = (items: number[]) => items.reduce(add, 0);\n\nexport const size = (items: number[]) => items.length;\n",
    ),
    (
        "cart/src/total.test.ts",
        "import { total } from \"./cart\";\nit(\"totals\", () => { expect(total([1])).toBe(1); });\n",
    ),
    (
        "cart/src/size.test.ts",
        "import { size } from \"./cart\";\nit(\"sizes\", () => { expect(size([1])).toBe(1); });\n",
    ),
    (
        "labels/package.json",
        r#"{"name":"@shop/labels","dependencies":{"@shop/money":"file:../money"},"scripts":{"test":"vitest run","build":"tsc"}}"#,
    ),
    (
        "labels/src/label.ts",
        "import { format } from \"@shop/money\";\n\nexport const label = (n: number) => format(n);\n",
    ),
    (
        "labels/src/label.test.ts",
        "import { label } from \"./label\";\nit(\"labels\", () => { expect(label(1)).toBe(\"$1\"); });\n",
    ),
];

#[test]
fn typescript_change_follows_package_imports_into_dependent_projects() {
    let ws = Workspace::new(TYPESCRIPT);
    ws.edit("money/src/math.ts", "return a + b;", "return b + a;");
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//money:test"),
        ["npm test -- src/math.test.ts"]
    );
    // The dependent that calls `add` runs the one test that reaches it; the
    // dependent that only formats is not run at all.
    assert_eq!(
        plan.commands("//cart:test"),
        ["npm test -- src/total.test.ts"]
    );
    assert_eq!(plan.runs(), ["//cart:test", "//money:test"]);
}

#[test]
fn build_runs_only_where_the_change_is_referenced() {
    let ws = Workspace::new(TYPESCRIPT);
    ws.edit("money/src/format.ts", "`$${n}`", "`USD ${n}`");
    let plan = ws.plan("build");
    assert_eq!(plan.runs(), ["//labels:build", "//money:build"]);
}

#[test]
fn lockfile_change_runs_dependents_in_full() {
    let ws = Workspace::new(TYPESCRIPT);
    ws.edit(
        "money/package.json",
        "\"build\":\"tsc\"",
        "\"build\":\"tsc -b\"",
    );
    let plan = ws.plan("test");
    assert_eq!(
        plan.runs(),
        ["//cart:test", "//labels:test", "//money:test"]
    );
    assert!(plan.commands("//cart:test").is_empty());
    assert!(plan
        .notes("//cart:test")
        .contains("depends on //money, whose change is not analysed"));
}

#[test]
fn go_change_selects_test_functions_by_name() {
    let ws = Workspace::new(&[
        ("svc/go.mod", "module example.com/svc\n\ngo 1.22\n"),
        (
            "svc/stock/stock.go",
            "package stock\n\ntype Shelf struct{ N int }\n\nfunc Hold(n int) int { return n }\n\nfunc (s Shelf) Count() int { return s.N }\n",
        ),
        (
            "svc/cart/cart.go",
            "package cart\n\nimport \"example.com/svc/stock\"\n\nfunc Reserve(n int) int { return stock.Hold(n) }\n\nfunc Size(items []int) int { return len(items) }\n\ntype Counter interface{ Count() int }\n\nfunc Tally(c Counter) int { return c.Count() }\n",
        ),
        (
            "svc/cart/cart_test.go",
            "package cart\n\nimport \"testing\"\n\nfunc TestReserve(t *testing.T) { Reserve(1) }\n\nfunc TestSize(t *testing.T) { Size(nil) }\n\nfunc TestTally(t *testing.T) { Tally(nil) }\n",
        ),
    ]);
    ws.edit("svc/stock/stock.go", "{ return n }", "{ return n + 0 }");
    assert_eq!(
        ws.plan("test").commands("//svc:test"),
        ["go test -run '^(TestReserve)$' ./cart"]
    );

    // A method is reached through an interface, without naming its package.
    ws.git(&["checkout", "-q", "."]);
    ws.edit("svc/stock/stock.go", "{ return s.N }", "{ return s.N + 0 }");
    assert_eq!(
        ws.plan("test").commands("//svc:test"),
        ["go test -run '^(TestTally)$' ./cart"]
    );
}

#[test]
fn go_names_are_told_apart_by_package_and_receiver() {
    let ws = Workspace::new(&[
        ("svc/go.mod", "module example.com/svc\n\ngo 1.22\n"),
        (
            "svc/box/box.go",
            "package box\n\nimport \"errors\"\n\ntype Lid struct{}\n\ntype Base struct{}\n\nfunc New() int { return 1 }\n\nfunc Fail() error { return errors.New(\"x\") }\n\nfunc (l *Lid) Close() int { return 1 }\n\nfunc (b *Base) Close() int { return 2 }\n\nfunc Shut(l *Lid) int { return l.Close() }\n\nfunc Rest(b *Base) int { return b.Close() }\n",
        ),
        (
            "svc/box/box_test.go",
            "package box\n\nimport \"testing\"\n\nfunc TestNew(t *testing.T) { New() }\n\nfunc TestFail(t *testing.T) { Fail() }\n\nfunc TestShut(t *testing.T) { Shut(nil) }\n\nfunc TestRest(t *testing.T) { Rest(nil) }\n",
        ),
    ]);
    // `errors.New` is another package's function, not this one's `New`.
    ws.edit(
        "svc/box/box.go",
        "{ return 1 }\n\nfunc Fail",
        "{ return 3 }\n\nfunc Fail",
    );
    assert_eq!(
        ws.plan("test").commands("//svc:test"),
        ["go test -run '^(TestNew)$' ./box"]
    );

    // `b.Close()` on a `*Base` cannot be `Lid`'s method.
    ws.git(&["checkout", "-q", "."]);
    ws.edit(
        "svc/box/box.go",
        "Close() int { return 1 }",
        "Close() int { return 4 }",
    );
    assert_eq!(
        ws.plan("test").commands("//svc:test"),
        ["go test -run '^(TestShut)$' ./box"]
    );
}

#[test]
fn a_method_reaches_only_the_tests_that_load_its_file() {
    let ws = Workspace::new(&[
        (
            "post/package.json",
            r#"{"name":"post","scripts":{"test":"vitest run"}}"#,
        ),
        (
            "post/src/mail.ts",
            "export class Mail {\n  send(text: string) {\n    return `mail:${text}`;\n  }\n}\n",
        ),
        (
            "post/src/sms.ts",
            "export class Sms {\n  send(text: string) {\n    return `sms:${text}`;\n  }\n}\n",
        ),
        // Calls `send` on whatever it is handed.
        (
            "post/src/notify.ts",
            "import path from \"node:path\";\n\nexport function notify(channel: { send(text: string): string }, parts: string[]) {\n  return channel.send(path.join(...parts) + [\"a\"].join(\"\"));\n}\n",
        ),
        (
            "post/src/mail.test.ts",
            "import { Mail } from \"./mail\";\nit(\"mails\", () => { expect(new Mail().send(\"x\")).toBe(\"mail:x\"); });\n",
        ),
        (
            "post/src/sms.test.ts",
            "import { Sms } from \"./sms\";\nit(\"texts\", () => { expect(new Sms().send(\"x\")).toBe(\"sms:x\"); });\n",
        ),
        (
            "post/src/notify-mail.test.ts",
            "import { Mail } from \"./mail\";\nimport { notify } from \"./notify\";\nit(\"notifies\", () => { notify(new Mail(), [\"x\"]); });\n",
        ),
        (
            "post/src/notify-sms.test.ts",
            "import { Sms } from \"./sms\";\nimport { notify } from \"./notify\";\nit(\"notifies\", () => { notify(new Sms(), [\"x\"]); });\n",
        ),
        // Loads a file whose name is computed: it may load `mail.ts`.
        (
            "post/src/plugin.test.ts",
            "import { notify } from \"./notify\";\nit(\"loads\", async () => { const m = await import(process.env.CHANNEL!); notify(new m.default(), [\"x\"]); });\n",
        ),
    ]);
    ws.edit("post/src/mail.ts", "`mail:${text}`", "`mail: ${text}`");
    // `sms.test.ts` and `notify-sms.test.ts` call a `send` too, but never
    // load `mail.ts`.
    assert_eq!(
        ws.plan("test").commands("//post:test"),
        ["npm test -- src/mail.test.ts src/notify-mail.test.ts src/plugin.test.ts"]
    );
}

#[test]
fn a_change_that_reaches_a_setup_file_runs_every_test() {
    let ws = Workspace::new(&[
        (
            "post/package.json",
            r#"{"name":"post","scripts":{"test":"vitest run"}}"#,
        ),
        (
            "post/vitest.config.ts",
            "export default { test: { setupFiles: [\"./test/setup.ts\"] } };\n",
        ),
        (
            "post/src/clock.ts",
            "export function now() {\n  return 1;\n}\n\nexport function later() {\n  return 2;\n}\n",
        ),
        // Runs before every test, none of which imports it.
        (
            "post/test/setup.ts",
            "import { now } from \"../src/clock\";\n\nglobalThis.started = now();\n",
        ),
        (
            "post/src/clock.test.ts",
            "import { later, now } from \"./clock\";\nit(\"now\", () => { expect(now()).toBe(1); });\nit(\"later\", () => { expect(later()).toBe(2); });\n",
        ),
        (
            "post/src/other.test.ts",
            "it(\"reads the start\", () => { expect(globalThis.started).toBe(1); });\n",
        ),
    ]);
    // The setup file does not use `later`.
    ws.edit("post/src/clock.ts", "return 2", "return 3");
    assert_eq!(
        ws.plan("test").commands("//post:test"),
        ["npm test -- src/clock.test.ts"]
    );

    ws.git(&["checkout", "-q", "."]);
    ws.edit("post/src/clock.ts", "return 1", "return 0");
    let plan = ws.plan("test");
    // No narrowed command: the target runs as written.
    assert!(plan.commands("//post:test").is_empty());
    assert!(
        plan.notes("//post:test")
            .contains("post/test/setup.ts is loaded before every test"),
        "{}",
        plan.notes("//post:test")
    );
}

#[test]
fn python_change_selects_test_files_through_imports() {
    let ws = Workspace::new(&[
        (
            "py/pyproject.toml",
            "[project]\nname = \"shop\"\nversion = \"0.1.0\"\n[tool.pytest.ini_options]\n",
        ),
        ("py/shop/__init__.py", ""),
        (
            "py/shop/money.py",
            "def add(a, b):\n    return a + b\n\n\ndef fmt(n):\n    return f\"${n}\"\n",
        ),
        (
            "py/tests/conftest.py",
            "import pytest\n\n\n@pytest.fixture\ndef wallet():\n    return 1\n",
        ),
        (
            "py/tests/test_add.py",
            "from shop.money import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n",
        ),
        (
            "py/tests/test_fmt.py",
            "from shop import money\n\n\ndef test_fmt(wallet):\n    assert money.fmt(wallet) == \"$1\"\n",
        ),
    ]);
    ws.edit("py/shop/money.py", "return a + b", "return b + a");
    assert_eq!(
        ws.plan("test").commands("//py:test"),
        ["pytest tests/test_add.py"]
    );

    // A fixture is used by parameter name, with no import of conftest.py.
    ws.git(&["checkout", "-q", "."]);
    ws.edit("py/tests/conftest.py", "return 1", "return 2");
    assert_eq!(
        ws.plan("test").commands("//py:test"),
        ["pytest tests/test_fmt.py"]
    );
}

#[test]
fn wrapper_targets_narrow_their_test_prerequisite() {
    let mut files = ELIXIR.to_vec();
    files.push((
        "shop/aster.toml",
        "[targets.test-ci]\ncommand = \"true\"\ndepends_on = [\"//self:test\"]\n\n[targets.check]\ncommand = \"bash -c 'mix test && mix credo'\"\n",
    ));
    let ws = Workspace::new(&files);
    ws.edit("shop/lib/shop/pricing.ex", "amount * 0.2", "amount * 0.25");

    let output = ws.aster(&[
        "affected",
        "test-ci",
        "--base=HEAD",
        "--related",
        "--dry-run",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("command: mix test test/shop/tax_test.exs"),
        "{stdout}"
    );

    // A command Aster cannot read as a test runner is left as written.
    let plan = ws.plan("check");
    assert!(plan.commands("//shop:check").is_empty());
    assert!(plan
        .notes("//shop:check")
        .contains("not a test command Aster can narrow"));
}

#[cfg(unix)]
#[test]
fn related_run_passes_selected_tests_to_a_files_placeholder() {
    use std::os::unix::fs::PermissionsExt;

    let mut files = ELIXIR.to_vec();
    files.push((
        "shop/aster.toml",
        "[targets.deps]\ncommand = \"true\"\n\n[targets.test]\ncommand = \"./record.sh {files}\"\ncapabilities = [\"files_list\"]\n",
    ));
    files.push((
        "shop/record.sh",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > ran.txt\n",
    ));
    let ws = Workspace::new(&files);
    let script = ws.root().join("shop/record.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    ws.git(&["commit", "-qam", "executable"]);
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );

    let output = ws.aster(&["affected", "test", "--base=HEAD", "--related", "--no-cache"]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let ran = fs::read_to_string(ws.root().join("shop/ran.txt")).unwrap();
    assert_eq!(ran.trim(), "test/shop/cart_test.exs");
}

/// The Elixir fixture plus two shard projects that each run half of its
/// tests from their own directory, and a wrapper that runs a named test
/// target of it.
fn sharded() -> Vec<(&'static str, &'static str)> {
    let mut files = ELIXIR.to_vec();
    files.push((
        "shop/aster.toml",
        "[targets.smoke]\ncommand = \"mix test test/shop/cart_test.exs test/shop/tax_test.exs\"\n",
    ));
    files.push((
        "shop-shard-1/package.json",
        r#"{"name":"shop-shard-1","private":true}"#,
    ));
    files.push((
        "shop-shard-1/aster.toml",
        "depends_on = [\"//shop\"]\n\n[targets.test-ci]\ncommand = \"bash -c 'cd ../shop && MIX_TEST_PARTITION=1 exec mix test --warnings-as-errors --partitions 2'\"\ndepends_on = [\"//shop:deps\"]\n",
    ));
    files.push((
        "shop-shard-2/package.json",
        r#"{"name":"shop-shard-2","private":true}"#,
    ));
    files.push((
        "shop-shard-2/aster.toml",
        "depends_on = [\"//shop\"]\n\n[targets.test-ci]\ncommand = \"bash -c 'cd ../shop && MIX_TEST_PARTITION=2 exec mix test --warnings-as-errors --partitions 2'\"\ndepends_on = [\"//shop:deps\"]\n",
    ));
    files.push((
        "shop-smoke/package.json",
        r#"{"name":"shop-smoke","private":true}"#,
    ));
    files.push((
        "shop-smoke/aster.toml",
        "depends_on = [\"//shop\"]\n\n[targets.test-ci]\ncommand = \"true\"\ndepends_on = [\"//shop:smoke\"]\n",
    ));
    files
}

#[test]
fn shard_projects_run_their_slice_of_the_selected_tests() {
    let ws = Workspace::new(&sharded());
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );
    let plan = ws.plan("test-ci");
    for shard in ["1", "2"] {
        let commands = plan.commands(&format!("//shop-shard-{shard}:test-ci"));
        assert_eq!(commands.len(), 1, "{plan:#?}");
        assert_eq!(
            shell_words::split(&commands[0]).unwrap(),
            [
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "cd ../shop && MIX_TEST_PARTITION={shard} exec mix test --warnings-as-errors \
                     --partitions 2 test/shop/cart_test.exs"
                ),
            ]
        );
    }
    let notes = plan.notes("//shop-shard-1:test-ci");
    assert!(
        notes.contains("sum (shop/lib/shop/pricing.ex:3)"),
        "{notes}"
    );
}

#[test]
fn shard_projects_are_skipped_when_no_test_reaches_the_change() {
    let ws = Workspace::new(&sharded());
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "# Sums the items.",
        "# Adds them up.",
    );
    let plan = ws.plan("test-ci");
    assert!(
        !plan
            .runs()
            .iter()
            .any(|addr| addr.contains("shard") || addr.contains("smoke")),
        "{plan:#?}"
    );
    // Nothing the shards run is affected, so they are not selected at all.
    assert!(
        plan.targets.keys().all(|addr| addr == "//shop:test-ci"),
        "{plan:#?}"
    );
}

#[test]
fn shard_projects_run_in_full_when_the_sharded_project_does() {
    let ws = Workspace::new(&sharded());
    ws.edit("shop/mix.exs", "0.1.0", "0.2.0");
    let plan = ws.plan("test-ci");
    assert!(plan.commands("//shop-shard-1:test-ci").is_empty());
    assert!(plan
        .notes("//shop-shard-1:test-ci")
        .contains("shop/mix.exs changed; running as written"));
}

#[test]
fn shard_projects_run_in_full_when_their_own_files_change() {
    let ws = Workspace::new(&sharded());
    ws.edit(
        "shop/lib/shop/pricing.ex",
        "Enum.sum(items)",
        "Enum.sum(items) + 0",
    );
    ws.edit(
        "shop-shard-1/aster.toml",
        "--partitions 2",
        "--partitions 2 --trace",
    );
    let plan = ws.plan("test-ci");
    assert!(plan.commands("//shop-shard-1:test-ci").is_empty());
    assert!(plan
        .notes("//shop-shard-1:test-ci")
        .contains("shop-shard-1/aster.toml changed; running in full"));
    // The other shard still narrows.
    assert_eq!(plan.commands("//shop-shard-2:test-ci").len(), 1);
}

#[test]
fn wrapper_projects_narrow_the_test_target_they_depend_on() {
    let ws = Workspace::new(&sharded());
    ws.edit("shop/lib/shop/pricing.ex", "amount * 0.2", "amount * 0.25");
    let output = ws.aster(&[
        "affected",
        "test-ci",
        "--base=HEAD",
        "--related",
        "--dry-run",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The named target lists two files; only the one that reaches the
    // change is kept.
    assert!(
        stdout.contains("command: mix test test/shop/tax_test.exs"),
        "{stdout}"
    );
    assert!(stdout.contains("//shop-smoke:test-ci"), "{stdout}");
}

#[test]
fn test_only_changes_do_not_run_dependents_in_other_languages() {
    let mut files = sharded();
    files.push((
        "portal/package.json",
        r#"{"name":"portal","scripts":{"test":"vitest run"}}"#,
    ));
    files.push(("portal/aster.toml", "depends_on = [\"//shop\"]\n"));
    files.push(("portal/src/a.test.ts", "it(\"runs\", () => {});\n"));
    let ws = Workspace::new(&files);
    // Only a test of the Elixir project changes: what it provides to the
    // JavaScript project that depends on it is the same.
    ws.edit("shop/test/shop/tax_test.exs", "== 2.0", "== 2.00");
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//shop:test"),
        ["mix test test/shop/tax_test.exs"]
    );
    assert!(!plan.runs().contains(&"//portal:test"), "{plan:#?}");

    // A change to its code does reach the dependent, which runs in full.
    ws.edit("shop/lib/shop/pricing.ex", "amount * 0.2", "amount * 0.25");
    let plan = ws.plan("test");
    assert!(plan
        .notes("//portal:test")
        .contains("depends on //shop, which is in another language"));
}

/// A Go gateway and an Elixir platform that depends on it without importing
/// it: only some of the platform's tests launch or read the gateway. Two
/// shard projects run the platform's suite, and a JavaScript portal depends
/// on the platform.
fn gateway_workspace(platform_toml: &str, portal_toml: &str) -> Vec<(String, String)> {
    let mut files: Vec<(&str, String)> = vec![
        ("gateway/go.mod", "module example.com/gateway\n\ngo 1.22\n".into()),
        (
            "gateway/main.go",
            "package main\n\nfunc Serve() int { return 1 }\n\nfunc main() { Serve() }\n".into(),
        ),
        ("gateway/configs/a.yaml", "name: a\n".into()),
        (
            "platform/mix.exs",
            "defmodule Platform.MixProject do\n  use Mix.Project\n  def project, do: [app: :platform, version: \"0.1.0\"]\nend\n".into(),
        ),
        ("platform/aster.toml", platform_toml.into()),
        (
            "platform/lib/platform/assets.ex",
            "defmodule Platform.Assets do\n  @dir Path.expand(\"../../../gateway/configs\", __DIR__)\n\n  def dir, do: @dir\nend\n".into(),
        ),
        (
            "platform/test/support/gateway.ex",
            "defmodule Platform.TestSupport.Gateway do\n  @root Path.expand(\"../../../gateway\", __DIR__)\n\n  def root, do: @root\nend\n".into(),
        ),
        (
            "platform/test/gateway_test.exs",
            "defmodule Platform.GatewayTest do\n  use ExUnit.Case\n  alias Platform.TestSupport.Gateway\n\n  test \"builds\" do\n    assert Gateway.root()\n  end\nend\n".into(),
        ),
        (
            "platform/test/direct_test.exs",
            "defmodule Platform.DirectTest do\n  use ExUnit.Case\n\n  test \"reads\" do\n    assert File.read!(\"gateway/configs/a.yaml\")\n  end\nend\n".into(),
        ),
        (
            "platform/test/assets_test.exs",
            "defmodule Platform.AssetsTest do\n  use ExUnit.Case\n\n  test \"dir\" do\n    assert Platform.Assets.dir()\n  end\nend\n".into(),
        ),
        (
            "platform/test/pure_test.exs",
            "defmodule Platform.PureTest do\n  use ExUnit.Case\n\n  test \"pure\" do\n    assert 1 + 1 == 2\n  end\nend\n".into(),
        ),
        ("platform-shard-1/package.json", r#"{"name":"platform-shard-1","private":true}"#.into()),
        (
            "platform-shard-1/aster.toml",
            "depends_on = [\"//platform\"]\n\n[targets.test-ci]\ncommand = \"bash -c 'cd ../platform && MIX_TEST_PARTITION=1 exec mix test --partitions 2'\"\n".into(),
        ),
        ("platform-shard-2/package.json", r#"{"name":"platform-shard-2","private":true}"#.into()),
        (
            "platform-shard-2/aster.toml",
            "depends_on = [\"//platform\"]\n\n[targets.test-ci]\ncommand = \"bash -c 'cd ../platform && MIX_TEST_PARTITION=2 exec mix test --partitions 2'\"\n".into(),
        ),
        (
            "portal/package.json",
            r#"{"name":"portal","scripts":{"test":"vitest run","test-ci":"vitest run"}}"#.into(),
        ),
        ("portal/aster.toml", portal_toml.into()),
        (
            "portal/src/schema.test.ts",
            "import { readFileSync } from \"fs\";\n\nit(\"reads\", () => {\n  readFileSync(\"../../platform/lib/platform/assets.ex\");\n});\n".into(),
        ),
        ("portal/src/pure.test.ts", "it(\"adds\", () => {\n  expect(1 + 1).toBe(2);\n});\n".into()),
    ];
    files.sort();
    files.into_iter().map(|(p, c)| (p.to_string(), c)).collect()
}

fn gateway(platform_toml: &str, portal_toml: &str) -> Workspace {
    let files = gateway_workspace(platform_toml, portal_toml);
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    Workspace::new(&files)
}

const GATEWAY_INFER: &str =
    "depends_on = [\"//gateway\"]\n\n[consumes.\"//gateway\"]\ninfer = true\n";
const PORTAL_INFER: &str =
    "depends_on = [\"//platform\"]\n\n[consumes.\"//platform\"]\ninfer = true\n";

fn change_gateway(ws: &Workspace) {
    ws.edit("gateway/main.go", "return 1", "return 2");
}

#[test]
fn cross_language_dependent_runs_only_the_tests_that_consume_the_dependency() {
    let ws = gateway(GATEWAY_INFER, "depends_on = [\"//platform\"]\n");
    change_gateway(&ws);
    let plan = ws.plan("test");
    // The helper that names the gateway's directory reaches the test that
    // uses it, as does the library module that names its configs; the test
    // that names a file of the gateway is found too. The pure test is not.
    assert_eq!(
        plan.commands("//platform:test"),
        ["mix test test/assets_test.exs test/direct_test.exs test/gateway_test.exs"],
        "{plan:#?}"
    );
    let notes = plan.notes("//platform:test");
    assert!(
        notes.contains("depends on //gateway, which is in another language"),
        "{notes}"
    );
    assert!(
        notes.contains("names gateway in //gateway, which changed"),
        "{notes}"
    );
    assert!(
        notes.contains("assets.ex ← names gateway/configs in //gateway, which changed"),
        "{notes}"
    );
    assert!(
        notes.contains("names gateway/configs/a.yaml in //gateway, which changed"),
        "{notes}"
    );
}

#[test]
fn dependent_without_evidence_runs_in_full() {
    // No entry: the dependent runs in full, and says how many definitions
    // name the dependency.
    let ws = gateway(
        "depends_on = [\"//gateway\"]\n",
        "depends_on = [\"//platform\"]\n",
    );
    change_gateway(&ws);
    let plan = ws.plan("test");
    assert!(plan.commands("//platform:test").is_empty(), "{plan:#?}");
    let notes = plan.notes("//platform:test");
    assert!(notes.contains("running in full"), "{notes}");
    assert!(
        notes.contains("definition(s) of the project name a path in //gateway")
            && notes.contains("infer = true"),
        "{notes}"
    );

    // An entry that finds nothing is no evidence either.
    let ws = gateway(GATEWAY_INFER, "depends_on = [\"//platform\"]\n");
    for file in [
        "test/support/gateway.ex",
        "test/direct_test.exs",
        "lib/platform/assets.ex",
    ] {
        fs::remove_file(ws.root().join("platform").join(file)).unwrap();
    }
    ws.git(&["add", "-A"]);
    ws.git(&["commit", "-q", "-m", "drop the evidence"]);
    change_gateway(&ws);
    let notes = ws.plan("test").notes("//platform:test");
    assert!(
        notes.contains("no definition of the project names a path in //gateway")
            && notes.contains("running in full"),
        "{notes}"
    );
}

#[test]
fn declared_files_narrow_without_inference() {
    let toml = "depends_on = [\"//gateway\"]\n\n[consumes.\"//gateway\"]\nfiles = [\"test/support/*.ex\"]\n";
    let ws = gateway(toml, "depends_on = [\"//platform\"]\n");
    change_gateway(&ws);
    let plan = ws.plan("test");
    assert_eq!(
        plan.commands("//platform:test"),
        ["mix test test/gateway_test.exs"],
        "{plan:#?}"
    );
    assert!(plan
        .notes("//platform:test")
        .contains("aster.toml declares platform/test/support/gateway.ex to consume //gateway"));

    // A pattern that matches nothing is not trusted.
    let toml = "depends_on = [\"//gateway\"]\n\n[consumes.\"//gateway\"]\nfiles = [\"test/support/gone.ex\"]\n";
    let ws = gateway(toml, "depends_on = [\"//platform\"]\n");
    change_gateway(&ws);
    let notes = ws.plan("test").notes("//platform:test");
    assert!(notes.contains("matches no source file"), "{notes}");
    assert!(notes.contains("running in full"), "{notes}");

    // An empty list states that nothing consumes the dependency.
    let toml = "depends_on = [\"//gateway\"]\n\n[consumes.\"//gateway\"]\nfiles = []\n";
    let ws = gateway(toml, "depends_on = [\"//platform\"]\n");
    change_gateway(&ws);
    assert!(!ws.plan("test").runs().contains(&"//platform:test"));
}

#[test]
fn shard_projects_follow_the_narrowed_dependent() {
    let ws = gateway(GATEWAY_INFER, "depends_on = [\"//platform\"]\n");
    change_gateway(&ws);
    let plan = ws.plan("test-ci");
    for shard in ["1", "2"] {
        let commands = plan.commands(&format!("//platform-shard-{shard}:test-ci"));
        assert_eq!(commands.len(), 1, "{plan:#?}");
        let words = shell_words::split(&commands[0]).unwrap();
        assert_eq!(
            words[2],
            format!(
                "cd ../platform && MIX_TEST_PARTITION={shard} exec mix test --partitions 2 \
                 test/assets_test.exs test/direct_test.exs test/gateway_test.exs"
            ),
            "{plan:#?}"
        );
    }
}

#[test]
fn narrowing_carries_on_to_the_dependents_of_the_dependent() {
    let ws = gateway(GATEWAY_INFER, PORTAL_INFER);
    change_gateway(&ws);
    // The platform's own code names the gateway, so what the platform
    // provides may differ and its dependent is affected in turn.
    let plan = ws.plan("test");
    let commands = plan.commands("//portal:test");
    assert_eq!(commands.len(), 1, "{plan:#?}");
    assert!(
        commands[0].contains("src/schema.test.ts") && !commands[0].contains("pure.test.ts"),
        "{commands:?}"
    );
    let notes = plan.notes("//portal:test");
    assert!(
        notes.contains("depends on //platform, which is in another language"),
        "{notes}"
    );

    // Without evidence the portal runs in full.
    let ws = gateway(GATEWAY_INFER, "depends_on = [\"//platform\"]\n");
    change_gateway(&ws);
    let plan = ws.plan("test");
    assert!(plan.commands("//portal:test").is_empty(), "{plan:#?}");
    assert!(plan
        .notes("//portal:test")
        .contains("depends on //platform, which is in another language"));
}

#[test]
fn consumes_entries_are_validated() {
    let ws = gateway(
        "depends_on = [\"//gateway\"]\n\n[consumes.\"//other\"]\ninfer = true\n",
        "",
    );
    let output = ws.aster(&["list"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not list in depends_on"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A Phoenix-shaped web app: two controllers behind one router, with a
/// request test for each and one test that names no path.
const WEB: &[(&str, &str)] = &[
    (
        "web/mix.exs",
        "defmodule Web.MixProject do\n  use Mix.Project\n  def project, do: [app: :web, version: \"0.1.0\"]\nend\n",
    ),
    (
        "web/lib/web/router.ex",
        "defmodule Web.Router do\n  use Web, :router\n\n  scope \"/api\", Web do\n    get \"/orders/:id\", OrderController, :show\n    get \"/users/:id\", UserController, :show\n  end\nend\n",
    ),
    (
        "web/lib/web/endpoint.ex",
        "defmodule Web.Endpoint do\n  use Phoenix.Endpoint, otp_app: :web\n\n  plug Web.Router\nend\n",
    ),
    (
        "web/lib/web/order_controller.ex",
        "defmodule Web.OrderController do\n  def show(conn, %{\"id\" => id}), do: Web.Orders.render(conn, id)\nend\n",
    ),
    (
        "web/lib/web/orders.ex",
        "defmodule Web.Orders do\n  def render(conn, id), do: {conn, id}\nend\n",
    ),
    (
        "web/lib/web/user_controller.ex",
        "defmodule Web.UserController do\n  def show(conn, %{\"id\" => id}), do: {conn, id}\nend\n",
    ),
    (
        "web/test/support/conn_case.ex",
        "defmodule Web.ConnCase do\n  use ExUnit.CaseTemplate\n\n  using do\n    quote do\n      @endpoint Web.Endpoint\n    end\n  end\nend\n",
    ),
    (
        "web/test/web/order_test.exs",
        "defmodule Web.OrderTest do\n  use Web.ConnCase\n\n  test \"shows\", %{conn: conn} do\n    assert get(conn, \"/api/orders/#{7}\")\n  end\nend\n",
    ),
    (
        "web/test/web/user_test.exs",
        "defmodule Web.UserTest do\n  use Web.ConnCase\n\n  test \"shows\", %{conn: conn} do\n    assert get(conn, \"/api/users/1\")\n  end\nend\n",
    ),
];

#[test]
fn a_controller_change_selects_the_tests_that_request_its_routes() {
    let ws = Workspace::new(WEB);
    // Two hops behind the route: the controller calls the changed function.
    ws.edit("web/lib/web/orders.ex", "{conn, id}", "{conn, id, :ok}");
    assert_eq!(
        ws.plan("test").commands("//web:test"),
        ["mix test test/web/order_test.exs"]
    );
}

#[test]
fn a_route_no_code_requests_by_path_selects_everything_behind_the_router() {
    let mut files = WEB.to_vec();
    files.retain(|(path, _)| *path != "web/test/web/order_test.exs");
    // The only test of the orders route builds its path at run time.
    files.push((
        "web/test/web/order_test.exs",
        "defmodule Web.OrderTest do\n  use Web.ConnCase\n\n  test \"shows\", %{conn: conn} do\n    assert get(conn, Enum.join([\"\", \"api\", \"orders\", \"7\"], \"/\"))\n  end\nend\n",
    ));
    let ws = Workspace::new(&files);
    ws.edit("web/lib/web/orders.ex", "{conn, id}", "{conn, id, :ok}");
    assert_eq!(
        ws.plan("test").commands("//web:test"),
        ["mix test test/web/order_test.exs test/web/user_test.exs"]
    );
}

#[test]
fn a_wrapper_in_a_project_with_its_own_code_does_not_follow_other_projects() {
    let mut files = sharded();
    // A JavaScript project whose test-ci groups its own tests with a test
    // target of the Elixir project.
    files.push((
        "tools/package.json",
        r#"{"name":"tools","scripts":{"test":"vitest run && tsc"}}"#,
    ));
    files.push((
        "tools/aster.toml",
        "[targets.test-ci]\ncommand = \"true\"\ndepends_on = [\"//self:test\", \"//shop:smoke\"]\n",
    ));
    files.push(("tools/src/a.ts", "export const a = 1;\n"));
    files.push((
        "tools/src/a.test.ts",
        "import { a } from \"./a\";\nit(\"a\", () => { expect(a).toBe(1); });\n",
    ));
    let ws = Workspace::new(&files);
    ws.edit("shop/lib/shop/pricing.ex", "amount * 0.2", "amount * 0.25");
    let plan = ws.plan("test-ci");
    // The code-free wrapper follows the shop; the project with code does not.
    assert!(plan.runs().contains(&"//shop-smoke:test-ci"), "{plan:#?}");
    assert!(!plan.runs().contains(&"//tools:test-ci"), "{plan:#?}");
}
