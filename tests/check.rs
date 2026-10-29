//! End-to-end tests for `aster check`: the rules, the output schema and the
//! exit status, through the real binary on a scratch workspace.

use serde_json::Value;
use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Workspace {
    tmp: TempDir,
}

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let workspace = Self {
            tmp: TempDir::new().unwrap(),
        };
        for (path, content) in files {
            workspace.write(path, content);
        }
        workspace
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.tmp.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn aster(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(self.tmp.path())
            .env("ASTER_CACHE_DIR", self.tmp.path().join(".cache"))
            .output()
            .unwrap()
    }

    /// `aster --json check <args>`: the exit status and the parsed report.
    fn check(&self, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["--json", "check"];
        all.extend_from_slice(args);
        let output = self.aster(&all);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json = serde_json::from_str(&stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON ({error}): {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), json)
    }
}

fn findings<'a>(report: &'a Value, rule: &str) -> Vec<&'a Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == rule)
        .collect()
}

/// `file:line` (or the target) of each finding of `rule`, sorted.
fn places(report: &Value, rule: &str) -> Vec<String> {
    let mut places: Vec<String> = findings(report, rule)
        .iter()
        .map(|f| match f["line"].as_u64() {
            Some(line) => format!("{}:{line}", f["file"].as_str().unwrap()),
            None => f["target"].as_str().unwrap().to_string(),
        })
        .collect();
    places.sort();
    places
}

const MIX: &str = "defmodule X.MixProject do\n  use Mix.Project\n  def project, do: [app: :x, version: \"0.1.0\"]\nend\n";

#[test]
fn a_literal_written_or_joined_onto_a_scratch_base_is_suspicious_in_every_language() {
    // Each test below names `data/fixture.json`, which the repository holds.
    // Line numbers are asserted, so keep the layout.
    let ws = Workspace::new(&[
        ("aster.toml", ""),
        ("data/fixture.json", "{}\n"),
        (
            "tests/shop_test.exs",
            "defmodule ShopTest do\n  use ExUnit.Case\n\n  test \"w\" do\n    File.write!(Path.join(tmp(), \"data/fixture.json\"), \"{}\")\n    File.read!(\"data/fixture.json\")\n    File.read!(Path.join(File.cwd!(), \"data/fixture.json\"))\n    # aster: not-a-read\n    File.write!(Path.join(tmp(), \"data/fixture.json\"), \"{}\")\n  end\nend\n",
        ),
        (
            "tests/shop.test.ts",
            "it(\"w\", () => {\n  writeFileSync(\"data/fixture.json\", x);\n  readFileSync(\"data/fixture.json\");\n  readFileSync(join(repoRoot, \"data/fixture.json\"));\n  mkdirSync(join(dir, \"data/fixture.json\")); // aster: not-a-read\n  copyFileSync(\"data/fixture.json\", out);\n  copyFileSync(src, \"data/fixture.json\");\n});\n",
        ),
        (
            "tests/test_shop.py",
            "def test_w(tmp_path):\n    os.makedirs('data/fixture.json')\n    open('data/fixture.json')\n    (tmp_path / 'data/fixture.json').write_text('x')\n    os.path.join(os.getcwd(), 'data/fixture.json')\n",
        ),
        (
            "tests/shop_test.go",
            "package tests\n\nfunc TestW(t *testing.T) {\n\tos.WriteFile(\"data/fixture.json\", nil, 0o644)\n\tos.ReadFile(\"data/fixture.json\")\n\tfilepath.Join(dir, \"data/fixture.json\")\n}\n",
        ),
        (
            "src/lib.rs",
            "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn w() {\n        std::fs::write(\"data/fixture.json\", \"{}\").unwrap();\n        std::fs::read(\"data/fixture.json\").unwrap();\n        let _ = dir.join(\"data/fixture.json\");\n    }\n}\n",
        ),
    ]);
    let (status, report) = ws.check(&["--rule", "suspicious-path-literal"]);
    assert_eq!(status, 0, "warnings alone do not fail: {report}");
    assert_eq!(
        places(&report, "suspicious-path-literal"),
        [
            "src/lib.rs:7",
            "src/lib.rs:9",
            "tests/shop.test.ts:2",
            "tests/shop.test.ts:7",
            "tests/shop_test.exs:5",
            "tests/shop_test.go:4",
            "tests/shop_test.go:6",
            "tests/test_shop.py:2",
            "tests/test_shop.py:4",
        ],
        "{report}"
    );
    let first = findings(&report, "suspicious-path-literal")[0];
    assert_eq!(first["severity"], "warning");
    let message = first["message"].as_str().unwrap();
    assert!(message.contains("aster: not-a-read"), "{message}");
    assert!(message.contains("data/fixture.json"), "{message}");

    // --strict turns the warnings into a failure.
    let (status, _) = ws.check(&["--rule", "suspicious-path-literal", "--strict"]);
    assert_eq!(status, 1);
}

#[test]
fn stale_annotations_are_errors() {
    let ws = Workspace::new(&[
        ("aster.toml", ""),
        ("data/fixture.json", "{}\n"),
        (
            "tests/shop_test.exs",
            "defmodule ShopTest do\n  use ExUnit.Case\n\n  test \"a\" do\n    # aster: not-a-read\n    File.write!(Path.join(tmp(), \"data/fixture.json\"), \"{}\")\n    # aster: not-a-read\n    x = 1 + 1\n    # aster: reads data/fixture.json\n    # aster: reads data/missing.json data/*.txt\n    # aster: nope\n    # aster: reads\n  end\nend\n",
        ),
    ]);
    let (status, report) = ws.check(&["--rule", "stale-annotation"]);
    assert_eq!(status, 1, "stale annotations are errors: {report}");
    assert_eq!(
        places(&report, "stale-annotation"),
        [
            "tests/shop_test.exs:10",
            "tests/shop_test.exs:11",
            "tests/shop_test.exs:12",
            "tests/shop_test.exs:7",
        ],
        "{report}"
    );
    let messages: Vec<&str> = findings(&report, "stale-annotation")
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert!(messages.iter().any(|m| m.contains("data/missing.json")));
    assert!(messages.iter().any(|m| m.contains("data/*.txt")));
    assert!(messages
        .iter()
        .any(|m| m.contains("unknown directive `aster: nope`")));
    assert!(messages.iter().any(|m| m.contains("names no path")));
    assert!(messages
        .iter()
        .any(|m| m.contains("holds no string literal")));
}

#[test]
fn consumes_entries_must_match_sources() {
    let ws = Workspace::new(&[
        ("aster.toml", ""),
        ("gw/mix.exs", MIX),
        ("shop/mix.exs", MIX),
        (
            "shop/aster.toml",
            "depends_on = [\"//gw\"]\n\n[consumes.\"//gw\"]\nfiles = [\"test/support/*.ex\", \"test/nothing_*.exs\"]\n",
        ),
        ("shop/test/support/harness.ex", "defmodule Harness do\nend\n"),
    ]);
    let (status, report) = ws.check(&["--rule", "bad-consumes"]);
    assert_eq!(status, 1, "{report}");
    let found = findings(&report, "bad-consumes");
    assert_eq!(found.len(), 1, "{report}");
    assert_eq!(found[0]["target"], "//shop");
    assert_eq!(found[0]["file"], "shop/aster.toml");
    assert!(found[0]["message"]
        .as_str()
        .unwrap()
        .contains("test/nothing_*.exs"));

    // A key missing from depends_on is a configuration error already.
    ws.write(
        "shop/aster.toml",
        "[consumes.\"//gw\"]\nfiles = [\"test/support/*.ex\"]\n",
    );
    let output = ws.aster(&["check"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn unnarrowable_test_commands_say_why_and_honour_wrappers() {
    let workspace = |wrappers: &str| {
        Workspace::new(&[
            ("aster.toml", wrappers),
            ("shop/mix.exs", MIX),
            (
                "shop/aster.toml",
                "[targets.test-chained]\ncommand = \"bash -c 'mix test --only slow && mix credo'\"\n\n[targets.test-script]\ncommand = \"bash -c 'mix test | tee out.log'\"\n\n[targets.test-plain]\ncommand = \"mix test --max-failures 1\"\n\n[targets.lint]\ncommand = \"bash -c 'mix credo && mix format --check-formatted'\"\n",
            ),
            (
                "web/package.json",
                r#"{"name":"web","scripts":{"test":"vitest run"}}"#,
            ),
            (
                "web/aster.toml",
                "[targets.test-ci]\ncommand = \"mise exec node@24 -- vitest run\"\n",
            ),
        ])
    };
    let (status, report) = workspace("").check(&["--rule", "unnarrowable-test-command"]);
    assert_eq!(status, 0);
    let reasons: Vec<(String, String)> = findings(&report, "unnarrowable-test-command")
        .iter()
        .map(|f| {
            (
                f["target"].as_str().unwrap().to_string(),
                f["message"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let targets: Vec<&str> = reasons.iter().map(|(t, _)| t.as_str()).collect();
    // The lint target holds no test runner, and the plain one narrows.
    assert_eq!(
        targets,
        ["//shop:test-chained", "//shop:test-script", "//web:test-ci"],
        "{reasons:?}"
    );
    assert!(reasons[0].1.contains("interpreter `bash`"), "{reasons:?}");
    assert!(
        reasons[0]
            .1
            .contains("runs in full whenever `--related` selects it"),
        "{reasons:?}"
    );
    assert!(
        reasons[2]
            .1
            .contains("`mise exec node@24`, which is not a configured wrapper"),
        "{reasons:?}"
    );

    // A configured wrapper makes the wrapped command narrowable again.
    let (_, report) =
        workspace("[affected.related]\nwrappers = [{ program = \"mise\", until = \"--\" }]\n")
            .check(&["--rule", "unnarrowable-test-command"]);
    let targets: Vec<&str> = findings(&report, "unnarrowable-test-command")
        .iter()
        .map(|f| f["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["//shop:test-chained", "//shop:test-script"]);
}

#[test]
fn test_targets_that_no_entry_reaches_are_orphans() {
    let files = |config: &str| {
        Workspace::new(&[
            ("aster.toml", config),
            ("shop/mix.exs", MIX),
            (
                "shop/aster.toml",
                "[targets.test-ci]\ncommand = \"mix test\"\ndepends_on = [\"//self:test-unit\"]\n\n[targets.test-unit]\ncommand = \"mix test test/unit\"\n\n[targets.test-smoke]\ncommand = \"mix test --only smoke\"\n\n[targets.test-one-file]\ncommand = \"mix test test/one_test.exs\"\n\n[targets.test-joined]\ncommand = \"mix test test/joined\"\nselected_by = [\"//shop:test-ci\"]\n",
            ),
            ("unlisted/mix.exs", MIX),
            (
                "unlisted/aster.toml",
                "[targets.test-ci]\ncommand = \"mix test\"\n",
            ),
        ])
    };
    let lanes =
        "[affected.lanes.shop]\ninclude = [\"//shop\"]\n\n[check]\nentries = [\"test-ci\"]\n";
    let (status, report) = files(lanes).check(&["--rule", "orphan-test-target"]);
    assert_eq!(status, 0);
    let found = places(&report, "orphan-test-target");
    assert!(
        found.contains(&"//shop:test-smoke".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"//unlisted:test-ci".to_string()),
        "{found:?}"
    );
    assert!(!found.contains(&"//shop:test-ci".to_string()), "{found:?}");
    assert!(
        !found.contains(&"//shop:test-unit".to_string()),
        "{found:?}"
    );
    assert!(
        !found.contains(&"//shop:test-joined".to_string()),
        "{found:?}"
    );
    let unlisted = findings(&report, "orphan-test-target")
        .into_iter()
        .find(|f| f["target"] == "//unlisted:test-ci")
        .unwrap();
    assert!(unlisted["message"]
        .as_str()
        .unwrap()
        .contains("no [affected.lanes]"));

    // --entry names the target without touching aster.toml.
    let ws = files("[affected.lanes.shop]\ninclude = [\"//shop\"]\n");
    let (_, report) = ws.check(&["--rule", "orphan-test-target"]);
    assert_eq!(report["skipped"][0]["rule"], "orphan-test-target");
    assert!(report["skipped"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("no entry target"));
    let (_, report) = ws.check(&["--rule", "orphan-test-target", "--entry", "test-ci"]);
    assert!(places(&report, "orphan-test-target").contains(&"//shop:test-smoke".to_string()));

    // Without lanes the rule is skipped, and says so.
    let (_, report) = files("").check(&["--rule", "orphan-test-target"]);
    assert!(findings(&report, "orphan-test-target").is_empty());
    assert!(report["skipped"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("no [affected.lanes]"));
}

#[test]
fn output_schema_exit_status_and_rule_selection() {
    let ws = Workspace::new(&[
        ("aster.toml", ""),
        ("shop/mix.exs", MIX),
        (
            "shop/aster.toml",
            "[targets.test-chained]\ncommand = \"bash -c 'mix test && mix credo'\"\n",
        ),
    ]);
    let (status, report) = ws.check(&[]);
    assert_eq!(status, 0, "warnings alone exit 0: {report}");
    assert_eq!(report["version"], 1);
    assert_eq!(report["summary"]["findings"], 1);
    assert_eq!(report["summary"]["warnings"], 1);
    assert_eq!(report["summary"]["errors"], 0);
    assert_eq!(report["summary"]["by_rule"]["unnarrowable-test-command"], 1);
    let finding = &report["findings"][0];
    for key in ["rule", "severity", "file", "line", "target", "message"] {
        assert!(finding.get(key).is_some(), "{key} missing: {finding}");
    }
    assert_eq!(finding["target"], "//shop:test-chained");
    assert!(report["stats"]["source_files"].is_u64());

    // Human output: the location, severity, rule id and message.
    let output = ws.aster(&["check"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("//shop:test-chained: warning[unnarrowable-test-command]"),
        "{text}"
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(ws.aster(&["check", "--strict"]).status.code(), Some(1));

    // Selecting another rule drops it.
    let (status, report) = ws.check(&["--rule", "stale-annotation", "--strict"]);
    assert_eq!(status, 0);
    assert_eq!(report["summary"]["findings"], 0);
}

#[test]
fn a_bad_configuration_exits_2_and_disable_skips_a_rule() {
    let ws = Workspace::new(&[("aster.toml", "[check]\ndisable = [\"no-such-rule\"]\n")]);
    let output = ws.aster(&["check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown rule `no-such-rule`"));

    let ws = Workspace::new(&[("aster.toml", "")]);
    assert_eq!(
        ws.aster(&["check", "--rule", "nope"]).status.code(),
        Some(2)
    );

    let ws = Workspace::new(&[("aster.toml", "this is not toml [")]);
    assert_eq!(ws.aster(&["check"]).status.code(), Some(2));

    // A rule listed in [check] disable is skipped.
    let ws = Workspace::new(&[
        (
            "aster.toml",
            "[check]\ndisable = [\"unnarrowable-test-command\"]\n",
        ),
        ("shop/mix.exs", MIX),
        (
            "shop/aster.toml",
            "[targets.test-chained]\ncommand = \"bash -c 'mix test && mix credo'\"\n",
        ),
    ]);
    let (_, report) = ws.check(&[]);
    assert_eq!(report["summary"]["findings"], 0);
}

type Files = Vec<(String, String)>;

/// A JavaScript project whose `test-ci` target is `ci_command` and which has
/// one extra target, `test-subset`, that no entry runs.
fn js_project(scripts: &str, ci_command: &str, subset: &str) -> Files {
    vec![
        (
            "package.json".into(),
            format!("{{\"name\":\"p\",\"scripts\":{{{scripts}}}}}"),
        ),
        (
            "aster.toml".into(),
            format!(
                "[targets.test-ci]\ncommand = \"{ci_command}\"\n\n[targets.test-subset]\ncommand = \"{subset}\"\n"
            ),
        ),
    ]
}

#[test]
fn a_package_script_that_runs_the_suite_covers_the_same_runner_on_some_files() {
    let cases: [(&str, Files, bool); 7] = [
        (
            "script is the runner",
            js_project(
                r#""test":"vitest run""#,
                "pnpm test",
                "pnpm exec vitest run src/a.test.ts",
            ),
            false,
        ),
        (
            "npm run, with flags",
            js_project(
                r#""test:ci":"vitest run --coverage""#,
                "npm run test:ci",
                "npx vitest run src/a.test.ts",
            ),
            false,
        ),
        (
            "the config file is not a test path, and a subset with the same config is covered",
            js_project(
                r#""test":"vitest run --coverage --config vitest.cov.config.ts""#,
                "pnpm test",
                "pnpm exec vitest run --config vitest.cov.config.ts src/a.test.ts",
            ),
            false,
        ),
        (
            "a subset with another config is reported",
            js_project(
                r#""test":"vitest run --config vitest.cov.config.ts""#,
                "pnpm test",
                "pnpm exec vitest run --config vitest.other.config.ts src/a.test.ts",
            ),
            true,
        ),
        (
            "a script that runs another runner does not cover vitest",
            js_project(
                r#""test":"mocha""#,
                "pnpm test",
                "pnpm exec vitest run src/a.test.ts",
            ),
            true,
        ),
        (
            "a subset that picks tests is reported",
            js_project(
                r#""test":"vitest run""#,
                "pnpm test",
                "pnpm exec vitest run --project web src/a.test.ts",
            ),
            true,
        ),
        (
            "no such package script",
            js_project(
                r#""lint":"eslint .""#,
                "pnpm test",
                "pnpm exec vitest run src/a.test.ts",
            ),
            true,
        ),
    ];
    for (name, files, reported) in cases {
        let ws = Workspace::new(&[(
            "aster.toml",
            "[affected.lanes.all]\ninclude = [\"//app\"]\n\n[check]\nentries = [\"test-ci\"]\n",
        )]);
        for (path, content) in &files {
            ws.write(&format!("app/{path}"), content);
        }
        let (_, report) = ws.check(&["--rule", "orphan-test-target"]);
        let found = places(&report, "orphan-test-target");
        assert_eq!(
            found.contains(&"//app:test-subset".to_string()),
            reported,
            "{name}: {found:?} {report}"
        );
    }
}

#[test]
fn an_alias_entry_reaches_the_target_it_aliases() {
    let ws = Workspace::new(&[
        (
            "aster.toml",
            "[affected.lanes.all]\ninclude = [\"//app\"]\n\n[check]\nentries = [\"test-ci\"]\n",
        ),
        ("app/package.json", "{\"name\":\"p\"}"),
        (
            "app/aster.toml",
            "[targets]\ntest-ci = { alias = \"test\" }\n\n[targets.test]\ncommand = \"pnpm exec vitest run src/a.test.ts\"\n\n[targets.test-other]\ncommand = \"pnpm exec vitest run src/b.test.ts\"\n",
        ),
    ]);
    let (_, report) = ws.check(&["--rule", "orphan-test-target"]);
    // `test-other` is not aliased by anything; its runner is not a whole suite either.
    assert_eq!(
        places(&report, "orphan-test-target"),
        ["//app:test-other"],
        "{report}"
    );
}

#[test]
fn a_workspace_wide_cargo_test_covers_the_member_crates() {
    let manifest = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
    };
    let orphans =
        |ci: &str| {
            let ws = Workspace::new(&[
            (
                "aster.toml",
                "[affected.lanes.rs]\ninclude = [\"//rs\"]\n\n[check]\nentries = [\"test-ci\"]\n",
            ),
            (
                "rs/Cargo.toml",
                &format!("{}\n[workspace]\nmembers = [\"crates/a\"]\n", manifest("root")),
            ),
            ("rs/src/lib.rs", "pub fn f() {}\n"),
            (
                "rs/aster.toml",
                &format!("[targets.test-ci]\ncommand = \"{ci}\"\n"),
            ),
            ("rs/crates/a/Cargo.toml", &manifest("a")),
            ("rs/crates/a/src/lib.rs", "pub fn f() {}\n"),
        ]);
            let (_, report) = ws.check(&["--rule", "orphan-test-target"]);
            places(&report, "orphan-test-target")
        };
    assert_eq!(orphans("cargo test --workspace"), Vec::<String>::new());
    assert_eq!(orphans("cargo test --all --locked"), Vec::<String>::new());
    assert_eq!(orphans("cargo test"), ["//rs/crates/a:test"]);
}

#[test]
fn a_reached_files_list_target_runs_the_whole_suite_of_its_runner() {
    let ws = Workspace::new(&[
        (
            "aster.toml",
            "[affected.lanes.all]\ninclude = [\"//app\"]\n\n[check]\nentries = [\"test-ci\"]\n",
        ),
        ("app/package.json", "{\"name\":\"p\"}"),
        (
            "app/aster.toml",
            "[targets.test-ci]\ncommand = \"true\"\ndepends_on = [\"//self:test\"]\n\n[targets.test]\ncommand = \"node related-tests.mjs vitest {files}\"\ncapabilities = [\"files_list\"]\n\n[targets.test-a]\ncommand = \"CI=true pnpm exec vitest run src/a.test.ts\"\n\n[targets.test-mocha]\ncommand = \"mocha test/a.js\"\n",
        ),
    ]);
    let (_, report) = ws.check(&["--rule", "orphan-test-target"]);
    let found = places(&report, "orphan-test-target");
    assert!(!found.contains(&"//app:test-a".to_string()), "{found:?}");
    assert!(found.contains(&"//app:test-mocha".to_string()), "{found:?}");
}

#[test]
fn a_reads_annotation_declares_the_literal_a_real_read() {
    // Lines asserted: keep the layout. Each test joins a literal onto a
    // scratch base, which is suspicious unless `reads` covers it.
    let src = "defmodule ShopTest do
  use ExUnit.Case

  test \"same line\" do
    File.read!(Path.join(base(), \"data/fixture.json\")) # aster: reads data/fixture.json
  end

  test \"line above\" do
    # aster: reads data/fixture.json
    File.read!(Path.join(base(), \"data/fixture.json\"))
  end

  test \"anywhere in the definition\" do
    File.read!(Path.join(base(), \"data/fixture.json\"))
    # aster: reads data/*.json
  end

  test \"a glob under the literal directory\" do
    # aster: reads deep/dir/scripts/*.sh
    File.read!(Path.join(base(), \"scripts/#{name()}.sh\"))
  end

  test \"a different path\" do
    # aster: reads data/other.json
    File.read!(Path.join(base(), \"data/fixture.json\"))
  end

  test \"declared in another definition\" do
    # aster: reads data/fixture.json
    :ok
  end

  test \"the literal of another definition\" do
    File.read!(Path.join(base(), \"data/fixture.json\"))
  end

  test \"suffix must be whole components\" do
    # aster: reads deep/dir/myscripts/*.sh
    File.read!(Path.join(base(), \"scripts/#{name()}.sh\"))
  end
end
";
    let ws = Workspace::new(&[
        ("aster.toml", ""),
        ("data/fixture.json", "{}\n"),
        ("data/other.json", "{}\n"),
        ("scripts/a.sh", "\n"),
        ("deep/dir/scripts/a.sh", "\n"),
        ("deep/dir/myscripts/a.sh", "\n"),
        ("tests/shop_test.exs", src),
    ]);
    let (status, report) = ws.check(&["--rule", "suspicious-path-literal"]);
    assert_eq!(status, 0, "{report}");
    assert_eq!(
        places(&report, "suspicious-path-literal"),
        [
            "tests/shop_test.exs:25",
            "tests/shop_test.exs:34",
            "tests/shop_test.exs:39",
        ],
        "{report}"
    );
    // Every reads entry matches something, so none is stale.
    let (status, report) = ws.check(&["--rule", "stale-annotation"]);
    assert_eq!(status, 0, "{report}");
}

#[test]
fn a_reads_annotation_that_matches_nothing_stays_stale_beside_the_literal() {
    let ws = Workspace::new(&[
        ("aster.toml", ""),
        ("data/fixture.json", "{}\n"),
        (
            "tests/shop_test.exs",
            "defmodule ShopTest do\n  use ExUnit.Case\n\n  test \"a\" do\n    # aster: reads data/missing.json\n    File.read!(Path.join(base(), \"data/fixture.json\"))\n  end\nend\n",
        ),
    ]);
    let (_, report) = ws.check(&["--rule", "stale-annotation"]);
    assert_eq!(
        places(&report, "stale-annotation"),
        ["tests/shop_test.exs:5"],
        "{report}"
    );
}

#[test]
fn allow_orphans_exempts_a_target_and_stale_entries_are_errors() {
    let ws = |allow: &str| {
        Workspace::new(&[
            (
                "aster.toml",
                &format!(
                    "[affected.lanes.shop]\ninclude = [\"//shop\"]\n\n[check]\nentries = [\"test-ci\"]\nallow_orphans = [{allow}]\n"
                ),
            ),
            ("shop/mix.exs", MIX),
            (
                "shop/aster.toml",
                "[targets.test-ci]\ncommand = \"mix test\"\n\n[targets.test-smoke]\ncommand = \"mix test --only smoke\"\n\n[targets.lint]\ncommand = \"mix format --check-formatted\"\n",
            ),
        ])
    };

    // An allow-listed orphan is not reported, and the entry is not stale.
    let (status, report) = ws("\"//shop:test-smoke\"").check(&[]);
    assert_eq!(status, 0, "{report}");
    assert!(
        findings(&report, "orphan-test-target").is_empty(),
        "{report}"
    );
    assert!(
        findings(&report, "stale-allow-orphan").is_empty(),
        "{report}"
    );

    // Without the entry the target is an orphan.
    let (_, report) = ws("").check(&["--rule", "orphan-test-target"]);
    assert_eq!(places(&report, "orphan-test-target"), ["//shop:test-smoke"]);

    // An address naming no target, a target that is not a test target, and a
    // target an entry reaches are stale errors.
    let (status, report) =
        ws("\"//shop:test-smoke\", \"//shop:nope\", \"//shop:lint\", \"//shop:test-ci\"")
            .check(&["--rule", "stale-allow-orphan"]);
    assert_eq!(status, 1, "{report}");
    assert_eq!(
        places(&report, "stale-allow-orphan"),
        ["//shop:lint", "//shop:nope", "//shop:test-ci"],
        "{report}"
    );
    assert_eq!(
        findings(&report, "stale-allow-orphan")[0]["severity"],
        "error"
    );
    let messages: Vec<&str> = findings(&report, "stale-allow-orphan")
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert!(messages[1].contains("names no test target"), "{messages:?}");
    assert!(messages[2].contains("already reaches"), "{messages:?}");

    // The stale check also runs when no rule is selected.
    let (status, _) = ws("\"//shop:nope\"").check(&[]);
    assert_eq!(status, 1);
}
