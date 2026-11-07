//! End-to-end artifact restore: a cache hit puts declared outputs back
//! without running the target again.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

/// Integration matrix: standalone and umbrella-child builds, warm and missing
/// dependency files, with an existing outputless success from the old policy.
/// The real CLI discovers targets, stores/reuses the seed entry, and starts child
/// processes. Only Mix/Elixir are shell fixtures: no Hex network or BEAM compiler
/// is exercised here. Their scripts do not implement Aster's cache decisions.
#[test]
#[cfg(unix)]
fn elixir_build_fetches_missing_dependencies_despite_old_cached_success() {
    use std::os::unix::fs::PermissionsExt;

    for umbrella in [false, true] {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let project = tmp.path().join("app");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("mix.lock"), "%{}\n").unwrap();
        fs::write(
            project.join("mix.exs"),
            if umbrella {
                "[apps_path: \"apps\"]\n"
            } else {
                "[app: :proof]\n"
            },
        )
        .unwrap();
        let (build_dir, address, command) = if umbrella {
            let child = project.join("apps/proof");
            fs::create_dir_all(&child).unwrap();
            fs::write(child.join("mix.exs"), "[app: :proof]\n").unwrap();
            (
                child,
                "//app/apps/proof:build",
                "mix do --app proof compile",
            )
        } else {
            (project.clone(), "//app:build", "mix compile")
        };
        // As in Platform, compile must run while deps uses its inferred policy.
        let build_config =
            format!("[targets.build]\ncommand = \"{command}\"\ncache = {{ enabled = false }}\n");
        fs::write(build_dir.join("aster.toml"), &build_config).unwrap();

        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for (name, script) in [
            ("elixir", "#!/bin/sh\necho 'Elixir fixture'\n"),
            (
                "mix",
                r#"#!/bin/sh
set -eu
case "$*" in
  deps.get)
    mkdir -p deps
    echo dependency > deps/package
    echo fetch >> "$COUNTER_FILE"
    ;;
  compile|'do --app proof compile')
    test -f deps/package || { echo 'dependency is not available' >&2; exit 1; }
    ;;
  *) echo "unexpected mix arguments: $*" >&2; exit 1 ;;
esac
"#,
            ),
        ] {
            let path = bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let counter = tmp.path().join("fetches");
        let run = |target: &str| {
            let output = Command::new(env!("CARGO_BIN_EXE_aster"))
                .current_dir(tmp.path())
                .env("PATH", &path)
                .env("COUNTER_FILE", &counter)
                .env("ASTER_CACHE_DIR", tmp.path().join("cache"))
                .env_remove("ASTER_SRV_URL")
                .env_remove("ASTER_REMOTE_CACHE")
                .args(["run", target, "--json"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "umbrella={umbrella}, {target}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
        };

        // Produce a legacy outputless entry through the real cache writer. An
        // explicit user policy can still enable caching; the inferred default
        // below must ignore this success after the native dependency tree goes.
        let root_build = if umbrella { "" } else { &build_config };
        fs::write(
            project.join("aster.toml"),
            format!(
                "{root_build}\n[targets.deps]\ncommand = \"mix deps.get\"\ncache = {{ enabled = true, outputs = [] }}\n"
            ),
        )
        .unwrap();
        run(address);
        let seeded = run("//app:deps");
        assert_eq!(seeded["summary"]["cached"], 1, "{seeded}");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "fetch\n");

        // Restore the inferred policy and simulate a cold native cache. Build
        // crosses the actual deps edge and cannot succeed without a new fetch.
        fs::write(project.join("aster.toml"), root_build).unwrap();
        fs::remove_dir_all(project.join("deps")).unwrap();
        let cold = run(address);
        assert_eq!(cold["summary"]["cached"], 0, "{cold}");
        assert_eq!(
            fs::read_to_string(project.join("deps/package")).unwrap(),
            "dependency\n"
        );
        assert_eq!(fs::read_to_string(&counter).unwrap(), "fetch\nfetch\n");

        // Warm files still go through Mix, which owns dependency validation.
        run(address);
        assert_eq!(
            fs::read_to_string(&counter).unwrap(),
            "fetch\nfetch\nfetch\n"
        );
    }
}

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

    let quiet_mode = run(&["--quiet"]);
    assert!(quiet_mode.status.success(), "{quiet_mode:?}");
    let quiet_output = format!(
        "{}{}",
        String::from_utf8_lossy(&quiet_mode.stdout),
        String::from_utf8_lossy(&quiet_mode.stderr)
    );
    assert!(
        quiet_output.contains("1 cached"),
        "quiet mode must still report the hit: {quiet_output}"
    );
    assert!(
        !quiet_output.contains("hello"),
        "quiet mode must not replay target output: {quiet_output}"
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

/// A cache hit must not take away outputs another process is reading.
///
/// The firstlanding coverage incident: one `aster` invocation restored the
/// SDK's cached `dist/` while tests in another process imported it, and a
/// third invocation restoring the same target failed with "Directory not
/// empty". Restore used to delete every declared output root and write it
/// back, so a hit on unchanged outputs still left a window where files were
/// missing. This drives real `aster run` processes against one checkout.
#[test]
fn concurrent_cache_hits_keep_unchanged_outputs_readable() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // Setup: a build that writes a nested `dist/` tree, declared as a
    // directory output, the shape a JS SDK build has.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_sdk_fixture(root);
    let counter = root.join("runs.txt");
    let aster = |root: &std::path::Path| run_sdk_build(root, &counter);
    let first = aster(root);
    assert!(first.status.success(), "{first:?}");
    let files: Vec<_> = walkdir_files(&root.join("sdk/dist"));
    assert_eq!(files.len(), 401);

    // Boundary: a separate reader keeps opening every module, standing in
    // for a test process importing the SDK while builds run elsewhere.
    let stop = Arc::new(AtomicBool::new(false));
    let missing = Arc::new(AtomicUsize::new(0));
    let reader = {
        let (stop, missing, files) = (stop.clone(), missing.clone(), files.clone());
        std::thread::spawn(move || {
            let mut reads = 0usize;
            while !stop.load(Ordering::Relaxed) {
                for file in &files {
                    if fs::read(file).is_err() {
                        missing.fetch_add(1, Ordering::Relaxed);
                    }
                    reads += 1;
                }
            }
            reads
        })
    };

    // Action: several concurrent `aster run` processes all hit the cache
    // and restore the same unchanged outputs.
    let runs: Vec<_> = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| (0..3).map(|_| aster(root)).collect::<Vec<_>>()))
            .collect();
        callers
            .into_iter()
            .flat_map(|caller| caller.join().unwrap())
            .collect()
    });
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().unwrap();

    // Outcome: every invocation succeeded from the cache without warning,
    // the build never re-ran, and the reader never saw a module disappear.
    for output in &runs {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "aster failed: {stderr}");
        assert!(
            !stderr.contains("cache restore failed"),
            "restore raced another restore: {stderr}"
        );
    }
    assert_eq!(
        fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "every later invocation must be a cache hit"
    );
    assert!(reads > 0);
    assert_eq!(
        missing.load(Ordering::Relaxed),
        0,
        "reader saw outputs vanish during {reads} reads"
    );
    assert_eq!(walkdir_files(&root.join("sdk/dist")), files);
}

/// Concurrent hits on a checkout whose outputs changed type since the
/// manifest was stored (a file became a directory and the reverse) all
/// succeed. One restore replacing a path must not fail another restore that
/// stat'd the old type, because a failed restore deletes its outputs.
#[test]
fn concurrent_cache_hits_converge_a_checkout_whose_output_types_changed() {
    // Setup: build once, then flip two outputs to the other type.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_sdk_fixture(root);
    let counter = root.join("runs.txt");
    assert!(run_sdk_build(root, &counter).status.success());
    let expected = walkdir_files(&root.join("sdk/dist"));
    let expected_index = fs::read(root.join("sdk/dist/index.js")).unwrap();

    for round in 0..5 {
        fs::remove_file(root.join("sdk/dist/index.js")).unwrap();
        fs::create_dir_all(root.join("sdk/dist/index.js/stale")).unwrap();
        fs::remove_dir_all(root.join("sdk/dist/resources/a")).unwrap();
        fs::write(root.join("sdk/dist/resources/a"), b"was a directory").unwrap();

        // Action: four `aster run` processes restore the same hit at once.
        let outputs: Vec<_> = std::thread::scope(|scope| {
            let callers: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| run_sdk_build(root, &counter)))
                .collect();
            callers
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .collect()
        });

        // Outcome: no restore failed, the build never re-ran, and the
        // checkout matches the manifest again.
        for output in &outputs {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "round {round}: {stderr}");
            assert!(
                !stderr.contains("cache restore failed"),
                "round {round}: a restore failed: {stderr}"
            );
        }
        assert_eq!(
            walkdir_files(&root.join("sdk/dist")),
            expected,
            "round {round}"
        );
        assert_eq!(
            fs::read(root.join("sdk/dist/index.js")).unwrap(),
            expected_index
        );
    }
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1);
}

fn write_sdk_fixture(root: &std::path::Path) {
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("sdk")).unwrap();
    fs::write(root.join("sdk/package.json"), r#"{"name":"sdk"}"#).unwrap();
    fs::write(
        root.join("sdk/aster.toml"),
        r#"
[targets.build]
command = "bash -c 'for d in a b c d; do mkdir -p dist/resources/$d; for i in $(seq 1 100); do echo $d$i > dist/resources/$d/m$i.js; done; done; echo index > dist/index.js; echo run >> \"$COUNTER_FILE\"'"
cache = { enabled = true, outputs = ["dist"] }
"#,
    )
    .unwrap();
}

fn run_sdk_build(root: &std::path::Path, counter: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(root)
        .env("COUNTER_FILE", counter)
        .args(["run", "//sdk:build", "--no-deps"])
        .output()
        .unwrap()
}

fn walkdir_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Planted `*.tmp.<deadpid>.<n>` files stand in for a writer killed during
/// `put_blob_once` / `write_atomic`. Prune must drop the ones older than the
/// grace and keep a fresh one. `cache clear` only touches the index, so it
/// leaves every temp file for prune.
#[test]
fn cache_prune_and_clear_remove_tmp_files_left_by_killed_writers() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let cache = TempDir::new().unwrap();
    let cache_dir = cache.path();

    let mut child = Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    assert!(child.wait().unwrap().success());

    let digest = "ab".repeat(32);
    let key = "cd0123456789abcd";
    let old_cas = cache_dir
        .join("cas")
        .join("ab")
        .join(format!("{digest}.zst.tmp.{dead_pid}.1"));
    let fresh_cas = cache_dir
        .join("cas")
        .join("ab")
        .join(format!("{digest}.zst.tmp.{dead_pid}.2"));
    let old_ac = cache_dir
        .join("ac")
        .join("cd")
        .join(format!("{key}.json.tmp.{dead_pid}.1"));
    let fresh_ac = cache_dir
        .join("ac")
        .join("cd")
        .join(format!("{key}.json.tmp.{dead_pid}.2"));
    for path in [&old_cas, &fresh_cas, &old_ac, &fresh_ac] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    fs::write(&old_cas, vec![1u8; 4_096]).unwrap();
    fs::write(&fresh_cas, vec![2u8; 128]).unwrap();
    fs::write(&old_ac, b"{\"stale\":true}").unwrap();
    fs::write(&fresh_ac, b"{\"pending\":true}").unwrap();

    let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    for path in [&old_cas, &old_ac] {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(ancient).unwrap();
    }

    let clear = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_CACHE_DIR", cache_dir)
        .args(["cache", "clear"])
        .output()
        .unwrap();
    assert!(
        clear.status.success(),
        "cache clear failed: {}",
        String::from_utf8_lossy(&clear.stderr)
    );
    assert!(
        String::from_utf8_lossy(&clear.stdout).contains("Blobs are kept until `aster cache prune`"),
        "stdout={}",
        String::from_utf8_lossy(&clear.stdout)
    );
    for path in [&old_cas, &fresh_cas, &old_ac, &fresh_ac] {
        assert!(path.is_file(), "clear must leave {}", path.display());
    }

    let prune = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env("ASTER_CACHE_DIR", cache_dir)
        .args(["cache", "prune", "--max-size", "1", "--max-age", "1s"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&prune.stdout).into_owned();
    assert!(
        prune.status.success(),
        "cache prune failed: {} {stdout}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(!old_cas.exists(), "stale cas temp must be swept");
    assert!(!old_ac.exists(), "stale ac temp must be swept");
    assert!(fresh_cas.is_file(), "fresh cas temp must be kept");
    assert!(fresh_ac.is_file(), "fresh ac temp must be kept");

    let fresh_bytes =
        fs::metadata(&fresh_cas).unwrap().len() + fs::metadata(&fresh_ac).unwrap().len();
    let expected =
        format!("Pruned 0 manifests, 0 blobs, and 2 temp files ({fresh_bytes} bytes remaining)");
    assert!(
        stdout.contains(&expected),
        "stdout={stdout} expected={expected}"
    );
    assert_eq!(
        fresh_bytes,
        on_disk_bytes(cache_dir),
        "bytes remaining must equal on-disk cache size"
    );
}

fn on_disk_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                total += entry.metadata().unwrap().len();
            }
        }
    }
    total
}
