#![allow(clippy::disallowed_macros)] // test diagnostics may use std print macros
//! End-to-end test for `aster analyze change`: the real binary reads two
//! revisions of a small TypeScript, Elixir and Rust workspace and prints
//! what a change-measuring tool needs (definitions, resolved edges, a diff).

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Copy the tracked tree at `from` (everything but `.git`) to `to`.
fn snapshot(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = entry.unwrap();
        let target = to.join(entry.path().strip_prefix(from).unwrap());
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).unwrap();
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn analyze(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(cwd)
        .args(["analyze", "change"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "aster analyze change failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn has_edge(edges: &Value, from: &str, to: &str) -> bool {
    edges["resolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["from"] == from && e["to"] == to)
}

#[test]
fn analyze_change_reports_a_new_cycle_and_a_reused_helper_across_ts_elixir_and_rust() {
    // Setup: a base revision with a pnpm package that exposes a helper
    // through an `exports` map, an app that imports its own files through
    // tsconfig `paths`, an Elixir ledger and a Rust crate. No aster.toml:
    // the command reads plain trees.
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    write(
        root,
        &[
            ("pnpm-workspace.yaml", "packages:\n  - packages/*\n  - apps/*\n"),
            (
                "packages/util/package.json",
                r#"{"name": "@demo/util", "exports": {".": "./src/index.ts"}}"#,
            ),
            (
                "packages/util/src/index.ts",
                "export function helper(x: number): number {\n  return x * 2;\n}\n",
            ),
            (
                "apps/web/package.json",
                r#"{"name": "@demo/web", "dependencies": {"@demo/util": "workspace:*"}}"#,
            ),
            (
                "apps/web/tsconfig.json",
                r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["src/*"]}}}"#,
            ),
            (
                "apps/web/src/a.ts",
                "import { b } from \"@/b\";\n\nexport function a(): number {\n  return b() + 1;\n}\n",
            ),
            (
                "apps/web/src/b.ts",
                "export function b(): number {\n  return 1;\n}\n",
            ),
            ("mix.exs", "defmodule Demo.MixProject do\n  use Mix.Project\nend\n"),
            (
                "lib/ledger.ex",
                "defmodule Ledger do\n  def total(items), do: Enum.sum(items)\nend\n",
            ),
            (
                "crates/calc/Cargo.toml",
                "[package]\nname = \"calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("crates/calc/src/lib.rs", "mod math;\npub use math::add;\n"),
            (
                "crates/calc/src/math.rs",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            ),
        ],
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    let trees = TempDir::new().unwrap();
    snapshot(root, &trees.path().join("base"));

    // The change: `b.ts` now imports `a.ts`, closing a cycle; `a.ts` calls
    // the package's existing helper and adds an export nothing uses; a test
    // uses `importOriginal<typeof import(...)>()` in a module mock, which
    // the TypeScript grammar cannot parse; Elixir and Rust each gain a caller of existing
    // code.
    write(
        root,
        &[
            (
                "apps/web/src/a.ts",
                "import { b } from \"@/b\";\nimport { helper } from \"@demo/util\";\n\nexport function a(): number {\n  return helper(b()) + 1;\n}\n\nexport function unused(): number {\n  return 0;\n}\n",
            ),
            (
                "apps/web/src/b.ts",
                "import { a } from \"./a\";\n\nexport function b(): number {\n  return 1;\n}\n\nexport function again(): number {\n  return a();\n}\n",
            ),
            (
                "apps/web/src/a.test.ts",
                "import { a } from \"./a\";\n\nvi.mock(\"./b\", async (importOriginal) => {\n  const original = await importOriginal<typeof import(\"./b\")>();\n  return { ...original };\n});\n\nexport function checksA() {\n  return a() === 2;\n}\n",
            ),
            (
                "lib/report.ex",
                "defmodule Report do\n  def summary(items), do: Ledger.total(items)\nend\n",
            ),
            ("crates/calc/src/lib.rs", "mod api;\nmod math;\npub use math::add;\n"),
            (
                "crates/calc/src/api.rs",
                "pub fn sum3(a: i32, b: i32, c: i32) -> i32 {\n    crate::math::add(crate::math::add(a, b), c)\n}\n",
            ),
        ],
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "change"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    snapshot(root, &trees.path().join("head"));

    // Action: run the real binary in both input forms. The revision form
    // reads the repository at the current directory through `git archive`.
    let by_revision = analyze(root, &["--base", &base, "--head", &head]);
    let base_tree = trees.path().join("base");
    let head_tree = trees.path().join("head");
    let by_tree = analyze(
        trees.path(),
        &[
            "--base-tree",
            base_tree.to_str().unwrap(),
            "--head-tree",
            head_tree.to_str().unwrap(),
        ],
    );

    // Both forms describe the same files, so they print the same bytes.
    assert_eq!(
        String::from_utf8_lossy(&by_revision),
        String::from_utf8_lossy(&by_tree)
    );
    let report: Value = serde_json::from_slice(&by_revision).unwrap();
    assert_eq!(report["schema"], "aster-change-v1");
    let (a, b) = ("apps/web/src/a.ts", "apps/web/src/b.ts");

    // A new cycle: the change adds b -> a, and a -> b (through the tsconfig
    // alias) is in both revisions.
    let added: BTreeSet<(String, String)> = report["diff"]["edges_added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["from"].as_str().unwrap().into(),
                e["to"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(
        added.contains(&(b.into(), a.into())),
        "edges added: {added:?}"
    );
    assert!(has_edge(&report["edges"]["base"], a, b));
    assert!(has_edge(&report["edges"]["head"], a, b));
    assert!(!has_edge(&report["edges"]["base"], b, a));

    // The helper call resolves through the package's `exports` map to a
    // definition the base already has, unchanged.
    let util = "packages/util/src/index.ts";
    let head_a = &report["revisions"]["head"][a];
    let import = head_a["imports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["names"].as_array().unwrap().iter().any(|n| n == "helper"))
        .expect("a.ts imports helper");
    assert_eq!(import["targets"], serde_json::json!([util]));
    let def_a = head_a["defs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "a")
        .unwrap();
    assert!(def_a["refs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "helper"));
    let helper_hash = |side: &str| {
        report["revisions"][side][util]["defs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == "helper")
            .map(|d| d["hash"].clone())
    };
    assert!(helper_hash("base").is_some());
    assert_eq!(helper_hash("base"), helper_hash("head"));
    assert!(report["diff"]["files"].get(util).is_none());

    // The new export shows as an added definition; the edited one as
    // changed.
    let diff_a = &report["diff"]["files"][a];
    assert_eq!(diff_a["status"], "modified");
    assert_eq!(diff_a["defs_added"], serde_json::json!(["unused"]));
    assert_eq!(diff_a["defs_changed"], serde_json::json!(["a"]));

    // The file the grammar cannot fully parse keeps its definitions, and
    // says where the error is.
    let test_file = &report["revisions"]["head"]["apps/web/src/a.test.ts"];
    assert_eq!(test_file["parse_error"], true, "{test_file}");
    assert_eq!(test_file["error_lines"], serde_json::json!([[4, 4]]));
    // The error is in the mock call, code outside every definition.
    assert_eq!(test_file["top"]["uncertain"], true);
    assert!(test_file["defs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["name"] == "checksA"));

    // Elixir: the new module's call to `Ledger.total` resolves to the
    // ledger's file.
    let summary = &report["revisions"]["head"]["lib/report.ex"]["defs"][0];
    assert_eq!(summary["name"], "summary");
    let call = summary["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["module"] == "Ledger")
        .expect("summary calls Ledger");
    assert_eq!(call["function"], "total");
    assert_eq!(call["targets"], serde_json::json!(["lib/ledger.ex"]));

    // Rust: the new module calls the existing `math::add`.
    assert!(has_edge(
        &report["edges"]["head"],
        "crates/calc/src/api.rs",
        "crates/calc/src/math.rs"
    ));
    assert_eq!(
        report["diff"]["files"]["crates/calc/src/api.rs"]["defs_added"],
        serde_json::json!(["sum3"])
    );
}

#[test]
fn analyze_change_refuses_a_half_given_pair() {
    let dir = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(dir.path())
        .args(["analyze", "change", "--base", "HEAD"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn a_change_cannot_hide_files_or_run_programs_through_git_attributes_or_ignore_files() {
    // Setup: a repository whose own configuration defines a smudge filter
    // that would leave a marker if anything ran it. The change adds files
    // that ignore files, `export-ignore`, `ident` and directory names would
    // hide or rewrite, plus edits that only some hashes would notice.
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let marker = repo.path().join("..").join(format!(
        "{}-smudged",
        root.file_name().unwrap().to_str().unwrap()
    ));
    git(root, &["init", "-q", "-b", "main"]);
    git(
        root,
        &[
            "config",
            "filter.pw.smudge",
            &format!("touch {}; cat", marker.display()),
        ],
    );
    let latin1_base: &[u8] = b"# -*- coding: latin-1 -*-\n# caf\xe9\ndef f():\n    return 1\n";
    let latin1_head: &[u8] = b"# -*- coding: latin-1 -*-\n# caf\xe9\nimport os\nos.system('x')\n";
    write(
        root,
        &[
            ("src/p.py", "def p():\n    a()\n    b()\n"),
            ("src/id.py", "# $Id$\ndef z():\n    return 1\n"),
            (
                "lib/two.ex",
                "defmodule A do\n  def run, do: 1\nend\n\ndefmodule B do\n  def run, do: 2\nend\n",
            ),
            (
                "crates/c/Cargo.toml",
                "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/c/src/lib.rs", "pub fn top() {}\n"),
        ],
    );
    fs::write(root.join("src/latin.py"), latin1_base).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    let trees = TempDir::new().unwrap();
    snapshot(root, &trees.path().join("base"));

    write(
        root,
        &[
            (
                ".gitattributes",
                "*.py filter=pw\nsrc/exp.py export-ignore\nsrc/id.py ident\n",
            ),
            (".gitignore", "gen/\n"),
            (".ignore", "src/ign.py\n"),
            ("src/exp.py", "def exp():\n    return 1\n"),
            ("src/ign.py", "def ign():\n    return 1\n"),
            ("src/deps/hidden.py", "def hidden():\n    return 1\n"),
            ("gen/g.py", "def g():\n    return 1\n"),
            // Same tokens, but `b()` now runs only when `a()` is true.
            ("src/p.py", "def p():\n    if a():\n        b()\n"),
            (
                "lib/two.ex",
                "defmodule Z do\n  def run, do: 0\nend\n\ndefmodule A do\n  def run, do: 1\nend\n\ndefmodule B do\n  def run, do: 2\nend\n",
            ),
            ("crates/c/src/lib.rs", "mod util;\nmod gone;\npub fn top() {}\n"),
            ("crates/c/src/util.rs", "pub fn u() {}\n"),
        ],
    );
    fs::write(root.join("src/latin.py"), latin1_head).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["add", "-f", "gen/g.py"]);
    git(root, &["commit", "-q", "-m", "change"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    // The commit's `ident` attribute must not rewrite what is checked out.
    snapshot(root, &trees.path().join("head"));

    // Action: the revision form, the snapshot form, and the head checkout
    // itself (a git work tree with ignore files) as the head tree.
    let by_revision = analyze(root, &["--base", &base, "--head", &head]);
    let base_tree = trees.path().join("base");
    let head_tree = trees.path().join("head");
    let by_tree = analyze(
        trees.path(),
        &[
            "--base-tree",
            base_tree.to_str().unwrap(),
            "--head-tree",
            head_tree.to_str().unwrap(),
        ],
    );
    let by_checkout = analyze(
        trees.path(),
        &[
            "--base-tree",
            base_tree.to_str().unwrap(),
            "--head-tree",
            root.to_str().unwrap(),
        ],
    );

    // Nothing ran, and all three forms agree byte for byte.
    assert!(!marker.exists(), "the smudge filter ran");
    assert_eq!(
        String::from_utf8_lossy(&by_revision),
        String::from_utf8_lossy(&by_tree)
    );
    assert_eq!(
        String::from_utf8_lossy(&by_revision),
        String::from_utf8_lossy(&by_checkout)
    );
    let report: Value = serde_json::from_slice(&by_revision).unwrap();
    let files = &report["diff"]["files"];

    // No ignore file, attribute or directory name hides an added file.
    for added in ["src/exp.py", "src/ign.py", "src/deps/hidden.py", "gen/g.py"] {
        assert_eq!(files[added]["status"], "added", "{added}: {files}");
    }
    // `ident` rewrote nothing.
    assert!(files.get("src/id.py").is_none(), "{files}");
    // Indentation is Python syntax.
    assert_eq!(files["src/p.py"]["defs_changed"], serde_json::json!(["p"]));
    // A file that is not UTF-8 is still compared, by its bytes.
    assert_eq!(report["revisions"]["head"]["src/latin.py"]["opaque"], true);
    assert_eq!(files["src/latin.py"]["top_changed"], true);
    // Elixir definitions are keyed by module, so adding one renumbers
    // nothing.
    // The new module adds its body and its function.
    assert_eq!(
        files["lib/two.ex"]["defs_added"],
        serde_json::json!(["Z", "Z.run"])
    );
    assert_eq!(files["lib/two.ex"]["defs_removed"], serde_json::json!([]));
    assert_eq!(files["lib/two.ex"]["defs_changed"], serde_json::json!([]));
    // `mod util;` is an edge; `mod gone;` names a file the tree lacks.
    assert!(has_edge(
        &report["edges"]["head"],
        "crates/c/src/lib.rs",
        "crates/c/src/util.rs"
    ));
    assert!(report["edges"]["head"]["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|u| u["from"] == "crates/c/src/lib.rs"
            && u["spec"] == "self::gone"
            && u["internal"] == true));

    // `--path .` is the whole tree.
    let whole = analyze(root, &["--base", &base, "--head", &head, "--path", "."]);
    assert_eq!(whole, by_revision);
}
