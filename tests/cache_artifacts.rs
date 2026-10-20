//! End-to-end artifact restore: a cache hit puts declared outputs back
//! without running the target again.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn setup(tmp: &TempDir) {
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::create_dir(tmp.path().join("app")).unwrap();
    fs::write(tmp.path().join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        tmp.path().join("app/aster.toml"),
        r#"
[targets.build]
command = "bash -c 'mkdir -p out && echo hi > out/file.txt && echo run >> \"$COUNTER_FILE\"'"
cache = { enabled = true, outputs = ["out/**"] }
"#,
    )
    .unwrap();
}

#[test]
fn restores_outputs_on_hit() {
    let tmp = TempDir::new().unwrap();
    setup(&tmp);
    let counter = tmp.path().join("runs.txt");

    let run = |extra: &[&str]| {
        let mut args = vec!["run", "//app:build", "--no-deps"];
        args.extend_from_slice(extra);
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .env("COUNTER_FILE", &counter)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "aster failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run(&[]);
    let original = fs::read(tmp.path().join("app/out/file.txt")).unwrap();
    assert_eq!(original, b"hi\n");
    fs::remove_dir_all(tmp.path().join("app/out")).unwrap();

    let second = run(&["--json"]);
    let restored = fs::read(tmp.path().join("app/out/file.txt")).unwrap();
    assert_eq!(restored, original, "hit must restore the original bytes");
    let runs = fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 1, "hit must not re-run: {runs:?}");

    let parsed: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert!(
        parsed["summary"]["cached"].as_u64().unwrap_or(0) >= 1,
        "second run should be a cache hit: {parsed}"
    );
    assert!(
        !tmp.path().join(".aster/cache/cache.json").exists(),
        "legacy cache.json must stay untouched"
    );

    fs::write(
        tmp.path().join(".aster/cache/cache.json"),
        "{\"targets\":{}}",
    )
    .unwrap();
    fs::remove_dir_all(tmp.path().join("app/out")).unwrap();
    run(&["--no-cache"]);
    assert_eq!(
        fs::read_to_string(&counter).unwrap().lines().count(),
        2,
        "--no-cache must run again"
    );
    let leftover = fs::read_to_string(tmp.path().join(".aster/cache/cache.json")).unwrap();
    assert_eq!(
        leftover, "{\"targets\":{}}",
        "cache.json must be left untouched"
    );
}

/// A Node build whose script writes `dist/` is restored on a hit even when
/// aster.toml never sets `outputs`. The command is the task's build script.
/// `package-lock.json` is written first: `npm install` rewrites a missing
/// lockfile during the build, and the postflight hash then refuses to store.
#[test]
fn node_build_restores_dist_without_config() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::create_dir(tmp.path().join("app")).unwrap();
    fs::write(
        tmp.path().join("app/package.json"),
        r#"{"name":"app","scripts":{"build":"mkdir -p dist && echo ok > dist/a.txt && echo run >> $COUNTER_FILE"}}"#,
    )
    .unwrap();
    let lock = Command::new("npm")
        .args(["install", "--package-lock-only", "--silent"])
        .current_dir(tmp.path().join("app"))
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "npm install --package-lock-only failed: {}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let counter = tmp.path().join("runs.txt");

    let run = |extra: &[&str]| {
        let mut args = vec!["run", "//app:build"];
        args.extend_from_slice(extra);
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .env("COUNTER_FILE", &counter)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "aster failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run(&[]);
    assert_eq!(
        fs::read(tmp.path().join("app/dist/a.txt")).unwrap(),
        b"ok\n"
    );
    fs::remove_dir_all(tmp.path().join("app/dist")).unwrap();

    run(&[]);
    assert_eq!(
        fs::read(tmp.path().join("app/dist/a.txt")).unwrap(),
        b"ok\n",
        "hit must restore dist/a.txt"
    );
    let runs = fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 1, "hit must not re-run: {runs:?}");
}

/// A user `outputs` list replaces the plugin default, including an empty list.
#[test]
fn user_outputs_replace_node_default() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::create_dir(tmp.path().join("app")).unwrap();
    fs::write(
        tmp.path().join("app/package.json"),
        r#"{"name":"app","scripts":{"build":"mkdir -p dist && echo ok > dist/a.txt"}}"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("app/aster.toml"),
        "[targets.build]\ncommand = \"npm run build\"\ncache = { outputs = [] }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["cache", "status", "//app:build", "--inputs"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "status failed: {} {stdout}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("outputs: (none)"),
        "empty user outputs must replace dist/**: {stdout}"
    );
}

#[test]
fn cache_hit_replays_stored_log_and_exit_status() {
    let tmp = TempDir::new().unwrap();
    setup_echo(&tmp);

    let run = |extra: &[&str]| {
        let mut args = vec!["run", "//app:build", "--no-deps"];
        args.extend_from_slice(extra);
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(tmp.path())
            .args(args)
            .output()
            .unwrap()
    };

    let first = run(&[]);
    assert!(
        first.status.success(),
        "first run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    // Live captured output is stored, not printed. The hit must replay it
    // through that same path (`aster logs`) and mark the status line.
    let stored = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["logs", "//app:build"])
        .output()
        .unwrap();
    let stored_log = String::from_utf8_lossy(&stored.stdout);
    assert!(
        stored_log.contains("hello"),
        "live run should store hello: {stored_log}"
    );

    let second = run(&[]);
    assert!(second.status.success(), "second run failed: {second:?}");
    let replayed = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        replayed.contains("hello"),
        "cache hit should replay the stored log: {replayed}"
    );
    assert!(
        replayed.contains("(cached)"),
        "cache hit status line should be marked cached: {replayed}"
    );

    let logged = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["logs", "//app:build"])
        .output()
        .unwrap();
    let logged_text = String::from_utf8_lossy(&logged.stdout);
    assert!(
        logged_text.contains("hello"),
        "hit should store the replayed log for aster logs: {logged_text}"
    );

    let json_run = run(&["--json"]);
    assert!(json_run.status.success(), "json run failed: {json_run:?}");
    let parsed: serde_json::Value = serde_json::from_slice(&json_run.stdout).unwrap();
    let hit = parsed["results"]
        .as_array()
        .and_then(|results| {
            results
                .iter()
                .find(|result| result["address"] == "//app:build")
        })
        .expect("build result");
    assert_eq!(hit["cached"], true, "{hit}");
    assert_eq!(hit["exit_code"], 0, "{hit}");
    assert!(hit["duration_ms"].as_u64().is_some(), "duration_ms: {hit}");
    assert!(
        hit["output"].as_str().unwrap_or("").contains("hello"),
        "json should include the stored log: {hit}"
    );
    let json_text = format!(
        "{}{}",
        String::from_utf8_lossy(&json_run.stdout),
        String::from_utf8_lossy(&json_run.stderr)
    );
    // The JSON document contains the log as a field; stderr must not replay it.
    assert!(
        !String::from_utf8_lossy(&json_run.stderr).contains("hello"),
        "json mode must not print the log: {json_text}"
    );

    let quiet = run(&["--no-replay"]);
    assert!(quiet.status.success(), "no-replay run failed: {quiet:?}");
    let quiet_text = format!(
        "{}{}",
        String::from_utf8_lossy(&quiet.stdout),
        String::from_utf8_lossy(&quiet.stderr)
    );
    assert!(
        !quiet_text.contains("hello"),
        "--no-replay must not print the stored log: {quiet_text}"
    );
    assert!(
        quiet_text.contains("(cached)"),
        "--no-replay still marks the status line: {quiet_text}"
    );

    let quiet_json = run(&["--json", "--no-replay"]);
    let quiet_parsed: serde_json::Value = serde_json::from_slice(&quiet_json.stdout).unwrap();
    let quiet_hit = quiet_parsed["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["address"] == "//app:build")
        .unwrap();
    assert_eq!(quiet_hit["cached"], true, "{quiet_hit}");
    assert_eq!(quiet_hit["exit_code"], 0, "{quiet_hit}");
    assert!(
        quiet_hit["duration_ms"].as_u64().is_some(),
        "duration still reported: {quiet_hit}"
    );
}

fn setup_echo(tmp: &TempDir) {
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::create_dir(tmp.path().join("app")).unwrap();
    fs::write(tmp.path().join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        tmp.path().join("app/aster.toml"),
        r#"
[targets.build]
command = "echo hello"
cache = { enabled = true }
"#,
    )
    .unwrap();
}

/// A cache built in checkout A restores in checkout B.
///
/// Both workspaces share `ASTER_CACHE_DIR` and have different absolute paths.
/// `gen` writes `out/gen.txt`; `build` depends on it and writes `out/build.txt`.
/// Each command appends one line to `$COUNTER_FILE`, which lives outside both
/// workspaces, so a hit in B cannot hide a re-run.
#[test]
fn cache_built_in_one_checkout_restores_in_another() {
    let parent = TempDir::new().unwrap();
    let workspace_a = parent.path().join("machine-a").join("checkout");
    let workspace_b = parent.path().join("other").join("machine-b").join("src");
    let shared = parent.path().join("shared-cache");
    let counter = parent.path().join("runs.txt");
    fs::create_dir_all(&shared).unwrap();
    assert_ne!(workspace_a, workspace_b);

    write_rich_fixture(&workspace_a);

    let run = |root: &std::path::Path, extra: &[&str]| {
        let mut args = vec!["run", "//app:build"];
        args.extend_from_slice(extra);
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(root)
            .env("ASTER_CACHE_DIR", &shared)
            .env("COUNTER_FILE", &counter)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "aster failed in {}: stdout={} stderr={}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run(&workspace_a, &[]);
    let gen_a = fs::read(workspace_a.join("app/out/gen.txt")).unwrap();
    let build_a = fs::read(workspace_a.join("app/out/build.txt")).unwrap();
    assert_eq!(gen_a, b"from-gen\n");
    assert_eq!(build_a, b"from-gen\n");
    assert_eq!(
        fs::read_to_string(&counter).unwrap().lines().count(),
        2,
        "gen and build each append once in A"
    );

    copy_sources(&workspace_a, &workspace_b);
    assert!(
        !workspace_b.join("app/out").exists(),
        "copy must not bring A's outputs along"
    );

    let second = run(&workspace_b, &["--json"]);
    assert_eq!(
        fs::read(workspace_b.join("app/out/gen.txt")).unwrap(),
        gen_a,
        "gen output must restore byte-identical"
    );
    assert_eq!(
        fs::read(workspace_b.join("app/out/build.txt")).unwrap(),
        build_a,
        "build output must restore byte-identical"
    );
    assert_eq!(
        fs::read_to_string(&counter).unwrap().lines().count(),
        2,
        "workspace B must not execute either command"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    let results = parsed["results"].as_array().expect("results");
    for address in ["//app:gen", "//app:build"] {
        let hit = results
            .iter()
            .find(|result| result["address"] == address)
            .unwrap_or_else(|| panic!("{address} missing from {parsed}"));
        assert_eq!(hit["cached"], true, "{address} must be a hit: {hit}");
        let output = hit["output"].as_str().unwrap_or("");
        let marker = if address.ends_with(":gen") {
            "generated"
        } else {
            "built"
        };
        assert!(
            output.contains(marker),
            "{address} json should replay the stored log: {hit}"
        );
    }
    let replayed = String::from_utf8_lossy(&second.stderr);
    assert!(
        !replayed.contains("generated") && !replayed.contains("built"),
        "json mode must not print replayed logs: {replayed}"
    );

    let logs = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(&workspace_b)
        .env("ASTER_CACHE_DIR", &shared)
        .args(["logs", "//app:build"])
        .output()
        .unwrap();
    let logged = String::from_utf8_lossy(&logs.stdout);
    assert!(
        logs.status.success() && logged.contains("built"),
        "B should replay the stored build log: {} {logged}",
        String::from_utf8_lossy(&logs.stderr)
    );
}

fn write_rich_fixture(root: &std::path::Path) {
    fs::create_dir_all(root.join("app")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        root.join("app/aster.toml"),
        r#"
[targets.gen]
command = "bash -c 'mkdir -p out && echo from-gen > out/gen.txt && echo generated && echo gen >> \"$COUNTER_FILE\"'"
cache = { enabled = true, outputs = ["out/gen.txt"] }

[targets.build]
command = "bash -c 'mkdir -p out && cp out/gen.txt out/build.txt && echo built && echo build >> \"$COUNTER_FILE\"'"
depends_on = ["//self:gen"]
cache = { enabled = true, outputs = ["out/build.txt"] }
"#,
    )
    .unwrap();
}

/// Copy sources only. Declared outputs under `out/` stay behind in A.
fn copy_sources(from: &std::path::Path, to: &std::path::Path) {
    fn walk(from: &std::path::Path, to: &std::path::Path, rel: &std::path::Path) {
        for entry in fs::read_dir(from.join(rel)).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == "out" {
                continue;
            }
            let child = rel.join(&name);
            if entry.file_type().unwrap().is_dir() {
                fs::create_dir_all(to.join(&child)).unwrap();
                walk(from, to, &child);
            } else {
                fs::create_dir_all(to.join(rel)).unwrap();
                fs::copy(from.join(&child), to.join(&child)).unwrap();
            }
        }
    }
    fs::create_dir_all(to).unwrap();
    walk(from, to, std::path::Path::new(""));
}
