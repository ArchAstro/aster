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
