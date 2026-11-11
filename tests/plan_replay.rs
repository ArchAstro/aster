#![allow(clippy::disallowed_macros)] // test diagnostics may use std print macros
//! `aster affected --plan-out` and `--plan-in`: a plan written by one run
//! stands in for the analysis of another, and only when it is for the same
//! change.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

/// Two projects, `a` and `b`, in lanes `first` and `second`. Each has a
/// `rec` target that appends its name to `ran.log`.
struct Workspace {
    tmp: TempDir,
    /// Outside the checkout, where the plan and the run log live: a file the
    /// checkout does not track would count as a change.
    out: TempDir,
}

impl Workspace {
    fn new() -> Self {
        let ws = Workspace {
            tmp: TempDir::new().unwrap(),
            out: TempDir::new().unwrap(),
        };
        ws.git(&["init", "-q", "-b", "main"]);
        ws.git(&["config", "user.email", "t@example.com"]);
        ws.git(&["config", "user.name", "t"]);
        ws.write(
            "aster.toml",
            "[affected.lanes.first]\ninclude = [\"//a\"]\n\n[affected.lanes.second]\ninclude = [\"//b\"]\n",
        );
        for name in ["a", "b"] {
            ws.write(
                &format!("{name}/package.json"),
                &format!(r#"{{"name":"{name}","private":true}}"#),
            );
            ws.write(
                &format!("{name}/aster.toml"),
                &format!(
                    "[targets.rec]\ncommand = \"sh -c 'echo {name} >> {}'\"\n",
                    ws.out.path().join("ran.log").display()
                ),
            );
        }
        ws.git(&["add", "-A"]);
        ws.git(&["commit", "-q", "-m", "initial"]);
        // The change both lanes select.
        ws.write("a/changed.txt", "x");
        ws.write("b/changed.txt", "x");
        ws
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn aster(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(self.root())
            .output()
            .unwrap()
    }

    fn plan_file(&self) -> String {
        self.out
            .path()
            .join("plan.json")
            .to_string_lossy()
            .into_owned()
    }

    /// Plan every lane into `plan.json`.
    fn write_plan(&self) {
        let output = self.aster(&[
            "--json",
            "affected",
            "rec",
            "--base=HEAD",
            "--matrix",
            "--plan-out",
            &self.plan_file(),
        ]);
        assert!(output.status.success(), "{output:?}");
    }

    fn ran(&self) -> Vec<String> {
        fs::read_to_string(self.out.path().join("ran.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn forget_runs(&self) {
        let _ = fs::remove_file(self.out.path().join("ran.log"));
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read_plan(ws: &Workspace) -> Value {
    serde_json::from_str(&fs::read_to_string(ws.plan_file()).unwrap()).unwrap()
}

#[test]
fn matrix_writes_every_lane_with_the_key_it_is_valid_for() {
    let ws = Workspace::new();
    ws.write_plan();
    let plan = read_plan(&ws);
    assert_eq!(plan["schema"], 1);
    let lanes: Vec<&str> = plan["lanes"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(lanes, ["first", "second"]);
    let key = &plan["key"];
    assert_eq!(key["aster_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(key["base_sha"].as_str().unwrap().len(), 40);
    assert_eq!(key["head_sha"].as_str().unwrap().len(), 40);
    assert_eq!(key["mode"], "changed");
    assert_eq!(key["target"], "rec");
    let first = &plan["lanes"]["first"]["plan"];
    assert_eq!(first["targets"][0]["address"], "//a:rec");
}

#[test]
fn a_lane_runs_from_the_plan_exactly_as_the_analysis_runs_it() {
    let ws = Workspace::new();
    ws.write_plan();

    let fresh = ws.aster(&["affected", "rec", "--base=HEAD", "--lane=first"]);
    assert!(fresh.status.success(), "{fresh:?}");
    let fresh_ran = ws.ran();
    ws.forget_runs();

    let replayed = ws.aster(&[
        "affected",
        "rec",
        "--base=HEAD",
        "--lane=first",
        "--plan-in",
        &ws.plan_file(),
    ]);
    assert!(replayed.status.success(), "{replayed:?}");
    assert!(
        stderr(&replayed).contains("replaying lane 'first'"),
        "{}",
        stderr(&replayed)
    );
    assert_eq!(ws.ran(), fresh_ran);
    assert_eq!(fresh_ran, ["a"], "lane first runs project a only");
}

#[test]
fn a_dry_run_replay_prints_the_json_the_analysis_prints() {
    let ws = Workspace::new();
    ws.write_plan();
    for lane in ["first", "second"] {
        let args = ["--json", "affected", "rec", "--base=HEAD", "--dry-run"];
        let lane_arg = format!("--lane={lane}");
        let fresh = ws.aster(&[&args[..], &[lane_arg.as_str()][..]].concat());
        let replayed = ws.aster(
            &[
                &args[..],
                &[lane_arg.as_str(), "--plan-in", &ws.plan_file()][..],
            ]
            .concat(),
        );
        // One target per lane, so no ordering is left to differ.
        assert_eq!(
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&fresh.stdout),
            "lane {lane}"
        );
        assert!(stderr(&replayed).contains("replaying"), "lane {lane}");
    }
}

/// A replay that cannot be trusted logs why and runs the analysis, which
/// then produces the same result as a run without `--plan-in`.
fn assert_falls_back(ws: &Workspace, plan: &str, extra: &[&str], reason: &str) {
    let mut args = vec!["affected", "rec", "--lane=first"];
    args.extend(extra);
    let without = ws.aster(&args);
    assert!(without.status.success(), "{without:?}");
    let expected = ws.ran();
    ws.forget_runs();

    args.extend(["--plan-in", plan]);
    let with = ws.aster(&args);
    let log = stderr(&with);
    assert!(with.status.success(), "{log}");
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("--plan-in")).collect();
    assert_eq!(lines.len(), 1, "one line about the plan: {log}");
    assert!(lines[0].contains(reason), "{reason:?} not in {log}");
    assert!(lines[0].contains("analysing instead"), "{log}");
    assert!(!log.contains("replaying lane"), "{log}");
    assert_eq!(ws.ran(), expected);
    ws.forget_runs();
}

#[test]
fn a_missing_plan_file_falls_back() {
    let ws = Workspace::new();
    assert_falls_back(
        &ws,
        "/nonexistent/plan.json",
        &["--base=HEAD"],
        "file does not exist",
    );
}

#[test]
fn an_unreadable_plan_falls_back() {
    let ws = Workspace::new();
    fs::write(ws.plan_file(), "{ not json").unwrap();
    assert_falls_back(&ws, &ws.plan_file(), &["--base=HEAD"], "cannot parse it");
    // A directory cannot be read as a file.
    let dir = ws.out.path().to_string_lossy().into_owned();
    assert_falls_back(&ws, &dir, &["--base=HEAD"], "cannot read it");
}

#[test]
fn a_plan_from_another_aster_falls_back() {
    let ws = Workspace::new();
    ws.write_plan();
    let text = fs::read_to_string(ws.plan_file()).unwrap();
    let version = format!("\"aster_version\": \"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(text.contains(&version));
    fs::write(
        ws.plan_file(),
        text.replace(&version, "\"aster_version\": \"0.0.1\""),
    )
    .unwrap();
    assert_falls_back(
        &ws,
        &ws.plan_file(),
        &["--base=HEAD"],
        "aster version differs",
    );
}

#[test]
fn a_plan_for_another_base_falls_back() {
    let ws = Workspace::new();
    ws.git(&["add", "-A"]);
    ws.git(&["commit", "-q", "-m", "second"]);
    ws.write("a/more.txt", "x");
    // The plan is made against HEAD, the run asks for HEAD~1.
    ws.write_plan();
    assert_falls_back(&ws, &ws.plan_file(), &["--base=HEAD~1"], "base differs");
}

#[test]
fn a_plan_for_another_head_falls_back() {
    let ws = Workspace::new();
    ws.git(&["add", "-A"]);
    ws.git(&["commit", "-q", "-m", "second"]);
    ws.git(&["checkout", "-q", "-b", "other", "HEAD~1"]);
    ws.write("a/other.txt", "x");
    ws.git(&["add", "-A"]);
    ws.git(&["commit", "-q", "-m", "other"]);
    ws.git(&["checkout", "-q", "main"]);
    let plan = |head: &str| {
        let output = ws.aster(&[
            "--json",
            "affected",
            "rec",
            "--base=main~1",
            &format!("--head={head}"),
            "--matrix",
            "--plan-out",
            &ws.plan_file(),
        ]);
        assert!(output.status.success(), "{output:?}");
    };
    plan("main");
    assert_falls_back(
        &ws,
        &ws.plan_file(),
        &["--base=main~1", "--head=other"],
        "head differs",
    );
}

#[test]
fn a_plan_for_another_mode_falls_back() {
    let ws = Workspace::new();
    ws.write_plan();
    assert_falls_back(
        &ws,
        &ws.plan_file(),
        &["--base=HEAD", "--dependents"],
        "mode differs",
    );
}

#[test]
fn a_plan_for_another_set_of_changed_files_falls_back() {
    let ws = Workspace::new();
    ws.write_plan();
    ws.write("a/new-since-the-plan.txt", "x");
    assert_falls_back(
        &ws,
        &ws.plan_file(),
        &["--base=HEAD"],
        "changed files differs",
    );
}

#[test]
fn a_lane_missing_from_the_plan_falls_back() {
    let ws = Workspace::new();
    let planned = ws.aster(&[
        "--json",
        "affected",
        "rec",
        "--base=HEAD",
        "--lane=second",
        "--dry-run",
        "--plan-out",
        &ws.plan_file(),
    ]);
    assert!(planned.status.success(), "{planned:?}");
    assert_eq!(
        read_plan(&ws)["lanes"].as_object().unwrap().len(),
        1,
        "only the planned lane is stored"
    );
    assert_falls_back(
        &ws,
        &ws.plan_file(),
        &["--base=HEAD"],
        "lane 'first' is not in the plan (it has: second)",
    );
}

#[test]
fn text_dry_run_and_several_lanes_analyse_instead_of_replaying() {
    let ws = Workspace::new();
    ws.write_plan();
    let text = ws.aster(&[
        "affected",
        "rec",
        "--base=HEAD",
        "--lane=first",
        "--dry-run",
        "--plan-in",
        &ws.plan_file(),
    ]);
    assert!(text.status.success(), "{text:?}");
    assert!(
        stderr(&text).contains("prints JSON only"),
        "{}",
        stderr(&text)
    );
    let several = ws.aster(&[
        "--json",
        "affected",
        "rec",
        "--base=HEAD",
        "--lane=first",
        "--lane=second",
        "--dry-run",
        "--plan-in",
        &ws.plan_file(),
    ]);
    assert!(several.status.success(), "{several:?}");
    assert!(
        stderr(&several).contains("exactly one --lane"),
        "{}",
        stderr(&several)
    );
}

#[test]
fn plan_out_needs_a_dry_run_and_excludes_plan_in() {
    let ws = Workspace::new();
    let output = ws.aster(&["affected", "rec", "--base=HEAD", "--plan-out", "p.json"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("add --dry-run or --matrix"),
        "{}",
        stderr(&output)
    );
    let both = ws.aster(&[
        "affected",
        "rec",
        "--dry-run",
        "--plan-out",
        "p.json",
        "--plan-in",
        "p.json",
    ]);
    assert!(!both.status.success());
    assert!(!ws.root().join("p.json").exists());
}

#[test]
fn a_plan_inside_the_checkout_counts_as_a_change_unless_it_is_under_dot_aster() {
    let ws = Workspace::new();
    for (path, replays) in [("plan.json", false), (".aster/plan.json", true)] {
        let plan = ws.root().join(path);
        let planned = ws.aster(&[
            "--json",
            "affected",
            "rec",
            "--base=HEAD",
            "--matrix",
            "--plan-out",
            plan.to_str().unwrap(),
        ]);
        assert!(planned.status.success(), "{planned:?}");
        let replay = ws.aster(&[
            "affected",
            "rec",
            "--base=HEAD",
            "--lane=first",
            "--plan-in",
            plan.to_str().unwrap(),
        ]);
        assert!(replay.status.success(), "{replay:?}");
        let log = stderr(&replay);
        assert_eq!(log.contains("replaying lane"), replays, "{path}: {log}");
        if !replays {
            assert!(log.contains("changed files differs"), "{path}: {log}");
        }
        ws.forget_runs();
    }
}

#[test]
fn every_run_of_affected_reports_where_its_time_went() {
    let ws = Workspace::new();
    let output = ws.aster(&["affected", "rec", "--base=HEAD", "--lane=first"]);
    let log = stderr(&output);
    let line = log
        .lines()
        .find(|l| l.starts_with("[aster] affected phases:"))
        .unwrap_or_else(|| panic!("no phases line: {log}"));
    for phase in [
        "discover",
        "changes",
        "graph",
        "related",
        "narrow",
        "quarantine",
        "total",
    ] {
        assert!(line.contains(phase), "{phase} missing from {line}");
    }
    let quiet = ws.aster(&["--quiet", "affected", "rec", "--base=HEAD", "--lane=first"]);
    assert!(!stderr(&quiet).contains("affected phases"));
}

#[test]
fn an_edited_lane_config_is_not_replayed_even_with_a_pinned_head() {
    let ws = Workspace::new();
    ws.git(&["add", "-A"]);
    ws.git(&["commit", "-q", "-m", "change"]);
    let range = ["--base=HEAD~1", "--head=HEAD"];
    let mut plan_args = vec!["--json", "affected", "rec"];
    plan_args.extend(range);
    plan_args.extend(["--matrix", "--plan-out"]);
    let file = ws.plan_file();
    plan_args.push(&file);
    assert!(ws.aster(&plan_args).status.success());

    // An uncommitted edit moves lane second from //b to //a. The commits
    // are unchanged, the selection is not.
    let config = fs::read_to_string(ws.root().join("aster.toml")).unwrap();
    ws.write(
        "aster.toml",
        &config.replace(
            "[affected.lanes.second]\ninclude = [\"//b\"]",
            "[affected.lanes.second]\ninclude = [\"//a\"]",
        ),
    );
    let replay = ws.aster(&[
        "affected",
        "rec",
        "--base=HEAD~1",
        "--head=HEAD",
        "--lane=second",
        "--plan-in",
        &file,
    ]);
    let log = stderr(&replay);
    assert!(replay.status.success(), "{log}");
    assert!(!log.contains("replaying lane"), "{log}");
    assert!(log.contains("analysing instead"), "{log}");
    // What analysis selects now: lane second is project a.
    assert_eq!(ws.ran(), ["a"]);
}
