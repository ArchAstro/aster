//! JSON output validation tests
//!
//! Tests that all commands with --json flag produce valid, parseable JSON output.

use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

use super::{setup_workspace, write_file, write_package_json};

/// Helper to create a workspace with multiple Node.js projects
fn setup_workspace_with_projects() -> TempDir {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Create two projects: core and api (api depends on core)
    write_package_json(&tmp, "libs/core/package.json", r#"{"name": "core"}"#);
    write_package_json(
        &tmp,
        "services/api/package.json",
        r#"{"name": "api", "dependencies": {"core": "file:../../libs/core"}}"#,
    );

    tmp
}

#[test]
fn test_list_json_outputs_valid_json() {
    let tmp = setup_workspace_with_projects();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "list"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    // Verify stdout is valid JSON array
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("Should be valid JSON array");

    // Should have at least 2 projects
    assert!(
        parsed.len() >= 2,
        "Expected at least 2 projects, got {}",
        parsed.len()
    );

    // Verify structure of each project
    for project in &parsed {
        assert!(project.get("address").is_some(), "Missing 'address' field");
        assert!(project.get("path").is_some(), "Missing 'path' field");
        assert!(project.get("plugin").is_some(), "Missing 'plugin' field");
        assert!(project.get("targets").is_some(), "Missing 'targets' field");
    }
}

#[test]
fn test_list_json_empty_workspace() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "list"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("Should be valid JSON array");

    assert!(
        parsed.is_empty(),
        "Expected empty array for empty workspace"
    );
}

#[test]
fn test_graph_json_outputs_valid_json() {
    let tmp = setup_workspace_with_projects();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "graph"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");

    // Verify structure has "nodes" and "edges" keys
    assert!(parsed.get("nodes").is_some(), "Missing 'nodes' key");
    assert!(parsed.get("edges").is_some(), "Missing 'edges' key");

    // Nodes should be an array
    let nodes = parsed.get("nodes").unwrap();
    assert!(nodes.is_array(), "nodes should be an array");

    // Edges should be an object (map of address -> dependencies)
    let edges = parsed.get("edges").unwrap();
    assert!(edges.is_object(), "edges should be an object");
}

#[test]
fn test_why_json_outputs_valid_json() {
    let tmp = setup_workspace_with_projects();

    // Use target addresses (with :target suffix)
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "why", "//services/api:deps", "//libs/core:deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");

    // Verify structure has "from", "to", "path" keys
    assert!(parsed.get("from").is_some(), "Missing 'from' key");
    assert!(parsed.get("to").is_some(), "Missing 'to' key");
    assert!(parsed.get("path").is_some(), "Missing 'path' key");
}

#[test]
fn test_why_json_no_path_found() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // Two unrelated projects
    write_package_json(&tmp, "a/package.json", r#"{"name": "a"}"#);
    write_package_json(&tmp, "b/package.json", r#"{"name": "b"}"#);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "why", "//a:deps", "//b:deps"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");

    // path should be null when no path exists
    assert_eq!(
        parsed.get("path").unwrap(),
        &Value::Null,
        "path should be null when no path exists"
    );
}

#[test]
fn test_logs_json_no_previous_run() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "logs"])
        .output()
        .unwrap();

    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output empty object for no previous run
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON");
    assert!(
        parsed.is_object() || parsed.is_null(),
        "Should be object or null for no previous run"
    );
}

#[test]
fn test_logs_json_specific_target_not_found() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "logs", "//nonexistent:target"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Command should succeed even for missing target"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output empty object for not found target
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON");
    assert!(parsed.is_object(), "Should be object for missing target");
}

#[test]
fn test_json_flag_before_subcommand() {
    let tmp = setup_workspace_with_projects();

    // --json must come before the subcommand (global flag)
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    cmd.current_dir(tmp.path())
        .args(["--json", "list"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("["));
}

#[test]
fn test_json_flag_produces_valid_json_for_all_commands() {
    let tmp = setup_workspace_with_projects();

    // Test all commands that support JSON output
    let test_cases = vec![
        vec!["--json", "list"],
        vec!["--json", "graph"],
        vec!["--json", "logs"],
    ];

    for args in test_cases {
        let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
        let output = cmd.current_dir(tmp.path()).args(&args).output().unwrap();

        assert!(
            output.status.success(),
            "Command {args:?} failed: {output:?}"
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let _: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("Command {args:?} did not produce valid JSON: {e}"));
    }
}

/// A pnpm workspace (members in pnpm-workspace.yaml) where the consumer depends on
/// the dependency via the `workspace:*` protocol. aster must infer a build edge so
/// the dependency builds before the consumer.
#[test]
fn test_pnpm_workspace_protocol_build_edge() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    // pnpm workspace root: members declared in pnpm-workspace.yaml, not package.json.
    write_file(&tmp, "pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n");
    write_file(&tmp, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");

    write_package_json(
        &tmp,
        "packages/dep/package.json",
        r#"{"name": "@scope/dep", "scripts": {"build": "tsc"}}"#,
    );
    write_package_json(
        &tmp,
        "packages/consumer/package.json",
        r#"{"name": "@scope/consumer", "scripts": {"build": "tsc"},
            "dependencies": {"@scope/dep": "workspace:*"}}"#,
    );

    // graph --json exposes target-level edges (target -> its dependencies).
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "graph"])
        .output()
        .unwrap();
    assert!(output.status.success(), "Command failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("Should be valid JSON object");
    let edges = parsed.get("edges").unwrap().as_object().unwrap();

    let consumer_build = edges
        .get("//packages/consumer:build")
        .and_then(|v| v.as_array())
        .expect("consumer:build node should exist");
    assert!(
        consumer_build.contains(&Value::String("//packages/dep:build".to_string())),
        "consumer:build should depend on //packages/dep:build; got {consumer_build:?}"
    );

    // why confirms there is a dependency path from the consumer's build to the dep's.
    let mut why = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let why_out = why
        .current_dir(tmp.path())
        .args([
            "--json",
            "why",
            "//packages/consumer:build",
            "//packages/dep:build",
        ])
        .output()
        .unwrap();
    assert!(why_out.status.success(), "why failed: {why_out:?}");
    let why_json: Value = serde_json::from_str(&String::from_utf8_lossy(&why_out.stdout)).unwrap();
    assert!(
        why_json.get("path").map(|p| !p.is_null()).unwrap_or(false),
        "expected a dependency path consumer:build -> dep:build; got {why_json}"
    );
}

/// `graph --json` reports the files a target reads from other projects
/// (`cache.workspace_include`, and `cache.include` globs that leave the
/// project), which its edges cannot show.
#[test]
fn test_graph_json_reports_workspace_inputs() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(
        &tmp,
        "apps/web/package.json",
        r#"{"name": "web", "scripts": {"build": "tsc", "lint": "eslint ."}}"#,
    );
    write_file(
        &tmp,
        "apps/web/aster.toml",
        r#"
[targets.build]
command = "tsc"

[targets.build.cache]
workspace_include = ["libs/schema/*.json"]
include = ["src/**", "../../libs/theme/catalog.json"]
"#,
    );

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "graph"])
        .output()
        .unwrap();
    assert!(output.status.success(), "Command failed: {output:?}");
    let parsed: Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let inputs = parsed["workspace_inputs"]
        .as_object()
        .expect("workspace_inputs map");
    assert_eq!(
        inputs.len(),
        1,
        "only //apps/web:build reads outside its project: {inputs:?}"
    );
    assert_eq!(
        inputs["//apps/web:build"],
        serde_json::json!(["libs/schema/*.json", "libs/theme/catalog.json"])
    );
}

/// A real sparse worktree (`git worktree add` + `git sparse-checkout set`,
/// which records core.sparseCheckout in the worktree's config.worktree): an
/// explicit dependency on a project outside the cone no longer fails
/// discovery, and the checked-out projects load.
#[test]
fn test_list_in_sparse_worktree_with_dependency_outside_cone() {
    let tmp = TempDir::new().unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    for (path, body) in [
        ("aster.toml", ""),
        (
            "app/package.json",
            r#"{"name": "app", "scripts": {"build": "tsc"}}"#,
        ),
        (
            "app/aster.toml",
            "[targets.deploy]\ncommand = \"true\"\ndepends_on = [\"//lib:build\"]\n",
        ),
        (
            "lib/package.json",
            r#"{"name": "lib", "scripts": {"build": "tsc"}}"#,
        ),
    ] {
        let full = repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--no-checkout",
            wt.to_str().unwrap(),
        ],
    );
    git(&wt, &["sparse-checkout", "set", "--cone", "app"]);
    git(&wt, &["checkout", "-q"]);
    assert!(!wt.join("lib").exists(), "lib must be outside the cone");

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(&wt)
        .args(["--json", "list"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "list failed in a sparse worktree: {output:?}"
    );
    let projects: Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let addresses: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["address"].as_str().unwrap())
        .collect();
    assert!(addresses.contains(&"//app"), "{addresses:?}");
    assert!(!addresses.contains(&"//lib"), "{addresses:?}");
}

fn setup_rust_and_node_workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);

    write_file(
        &tmp,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
    );
    write_file(&tmp, "Cargo.lock", "# lock v1\n");
    write_file(
        &tmp,
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write_file(
        &tmp,
        "crates/b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
    );
    write_package_json(&tmp, "apps/web/package.json", "{\"name\": \"web\"}");
    write_file(
        &tmp,
        "apps/web/package-lock.json",
        "{\"lockfileVersion\": 3}\n",
    );

    tmp
}

fn cache_plan_json(tmp: &TempDir, extra: &[&str]) -> Value {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .args(["--json", "ci", "cache-plan"])
        .args(extra)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cache-plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cache-plan --json should be an object")
}

fn entry<'a>(plan: &'a Value, id: &str) -> &'a Value {
    plan["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("missing entry {id}: {plan}"))
}

fn assert_key_prefix(key: &str, id: &str) {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let prefix = format!("aster-l0-{id}-{os}-{arch}-");
    assert!(key.starts_with(&prefix), "{key} should start with {prefix}");
    let digest = &key[prefix.len()..];
    assert_eq!(digest.len(), 64, "sha256 hex digest: {key}");
    assert!(digest.chars().all(|ch| ch.is_ascii_hexdigit()));
}

#[test]
fn cache_plan_json_dedupes_shared_rust_caches_and_keeps_node() {
    let tmp = setup_rust_and_node_workspace();
    let plan = cache_plan_json(&tmp, &[]);

    let ids: Vec<&str> = plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            "cargo-registry",
            "cargo-target",
            "node-modules:apps/web",
            "npm-cache"
        ]
    );

    let registry = entry(&plan, "cargo-registry");
    assert_eq!(
        registry["paths"],
        serde_json::json!([
            "~/.cargo/registry/index",
            "~/.cargo/registry/cache",
            "~/.cargo/git/db"
        ])
    );
    assert_eq!(
        registry["projects"],
        serde_json::json!(["//crates/a", "//crates/b"])
    );
    assert_key_prefix(registry["key"].as_str().unwrap(), "cargo-registry");
    assert_eq!(
        registry["restore_keys"],
        serde_json::json!([format!(
            "aster-l0-cargo-registry-{}-{}-",
            std::env::consts::OS,
            std::env::consts::ARCH
        )])
    );

    let target = entry(&plan, "cargo-target");
    assert_eq!(target["paths"], serde_json::json!(["target"]));
    assert_eq!(
        target["projects"],
        serde_json::json!(["//crates/a", "//crates/b"])
    );
    assert_key_prefix(target["key"].as_str().unwrap(), "cargo-target");

    let modules = entry(&plan, "node-modules:apps/web");
    assert_eq!(
        modules["paths"],
        serde_json::json!(["apps/web/node_modules"])
    );
    assert_eq!(modules["projects"], serde_json::json!(["//apps/web"]));
    assert_key_prefix(modules["key"].as_str().unwrap(), "node-modules:apps/web");

    let npm = entry(&plan, "npm-cache");
    assert_eq!(npm["paths"], serde_json::json!(["~/.npm"]));
    assert_eq!(npm["projects"], serde_json::json!(["//apps/web"]));

    let combined_paths = plan["combined"]["paths"].as_array().unwrap();
    assert_eq!(combined_paths.len(), 6);
    assert!(combined_paths.contains(&serde_json::json!("~/.npm")));
    assert!(combined_paths.contains(&serde_json::json!("target")));
    assert_key_prefix(plan["combined"]["key"].as_str().unwrap(), "all");
    assert_eq!(
        plan["combined"]["restore_keys"],
        serde_json::json!([format!(
            "aster-l0-all-{}-{}-",
            std::env::consts::OS,
            std::env::consts::ARCH
        )])
    );
}

#[test]
fn cache_plan_lockfile_edit_changes_only_rust_keys() {
    let tmp = setup_rust_and_node_workspace();
    let before = cache_plan_json(&tmp, &[]);

    write_file(&tmp, "Cargo.lock", "# lock v2\n");
    let after = cache_plan_json(&tmp, &[]);

    for id in ["cargo-registry", "cargo-target"] {
        assert_ne!(
            entry(&before, id)["key"],
            entry(&after, id)["key"],
            "{id} key should change when Cargo.lock changes"
        );
    }
    for id in ["npm-cache", "node-modules:apps/web"] {
        assert_eq!(
            entry(&before, id)["key"],
            entry(&after, id)["key"],
            "{id} key should not change when Cargo.lock changes"
        );
    }
    assert_ne!(before["combined"]["key"], after["combined"]["key"]);
}

#[test]
fn cache_plan_selects_one_project() {
    let tmp = setup_rust_and_node_workspace();
    let plan = cache_plan_json(&tmp, &["//apps/web"]);
    let ids: Vec<&str> = plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["node-modules:apps/web", "npm-cache"]);
}

#[test]
fn cache_plan_keeps_independent_shared_caches_separate() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    // Two reactors share the Maven repository path but hash different poms.
    // Merging them would make one pom miss the cache for the other.
    write_file(
        &tmp,
        "services/a/pom.xml",
        "<project><artifactId>a</artifactId></project>\n",
    );
    write_file(
        &tmp,
        "services/b/pom.xml",
        "<project><artifactId>b</artifactId></project>\n",
    );

    let plan = cache_plan_json(&tmp, &[]);
    let repos: Vec<&Value> = plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["id"] == "maven-repository")
        .collect();
    assert_eq!(repos.len(), 2, "each reactor keeps its own pom key: {plan}");
    assert_ne!(repos[0]["key"], repos[1]["key"]);
    assert_eq!(repos[0]["paths"], serde_json::json!(["~/.m2/repository"]));
    assert_eq!(repos[1]["paths"], serde_json::json!(["~/.m2/repository"]));

    write_file(
        &tmp,
        "services/b/pom.xml",
        "<project><artifactId>b2</artifactId></project>\n",
    );
    let after = cache_plan_json(&tmp, &[]);
    let after_repos: Vec<&Value> = after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["id"] == "maven-repository")
        .collect();
    let before_a = repos
        .iter()
        .find(|entry| entry["projects"] == serde_json::json!(["//services/a"]))
        .unwrap();
    let after_a = after_repos
        .iter()
        .find(|entry| entry["projects"] == serde_json::json!(["//services/a"]))
        .unwrap();
    let after_b = after_repos
        .iter()
        .find(|entry| entry["projects"] == serde_json::json!(["//services/b"]))
        .unwrap();
    assert_eq!(after_a["key"], before_a["key"]);
    assert_ne!(after_b["key"], before_a["key"]);
}

#[test]
fn cache_plan_github_output_writes_combined_entry() {
    let tmp = setup_rust_and_node_workspace();
    let output_path = tmp.path().join("github-output.txt");

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("aster");
    let output = cmd
        .current_dir(tmp.path())
        .env("GITHUB_OUTPUT", &output_path)
        .args(["ci", "cache-plan", "--github-output"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cache-plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let written = std::fs::read_to_string(&output_path).unwrap();
    let plan = cache_plan_json(&tmp, &[]);
    let key = plan["combined"]["key"].as_str().unwrap();
    let restore = plan["combined"]["restore_keys"][0].as_str().unwrap();
    assert!(written.starts_with("paths<<ASTER_CACHE_PATHS\n"));
    assert!(written.contains("\nASTER_CACHE_PATHS\n"));
    assert!(written.contains(&format!("key={key}\n")));
    assert!(written.contains(&format!("restore-keys={restore}\n")));
    assert!(written.contains("~/.cargo/registry/index\n"));
    assert!(written.contains("apps/web/node_modules\n"));
}

/// `cache status --inputs --json` lists every key component, hashes env
/// values, and prints the same key execution stored.
#[test]
fn cache_status_inputs_json_matches_executor_hash() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "p/package.json", "{\"name\":\"p\"}\n");
    write_file(
        &tmp,
        "p/aster.toml",
        "[targets.build]\ncommand = \"true\"\n\n[targets.build.cache]\nenabled = true\ninclude = [\"package.json\"]\nenv = [\"APP_SECRET\", \"UNSET_CACHE_INPUT\"]\nruntime = [\"echo toolchain-1\"]\n",
    );

    let run = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .env("APP_SECRET", "s3cret-value")
        .env_remove("UNSET_CACHE_INPUT")
        .args(["run", "//p:build", "--no-deps"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let stored = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["--json", "cache", "status", "//p:build"])
        .output()
        .unwrap();
    assert!(stored.status.success(), "{stored:?}");
    let entry: Value = serde_json::from_slice(&stored.stdout).expect("cache status json");
    let stored_hash = entry["targets"][0]["key"]
        .as_str()
        .expect("stored key")
        .to_string();

    let status = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .env("APP_SECRET", "s3cret-value")
        .env_remove("UNSET_CACHE_INPUT")
        .args(["--json", "cache", "status", "//p:build", "--inputs"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success(),
        "cache status --inputs failed: {} {stdout}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        !stdout.contains("s3cret-value"),
        "env values must not be printed raw: {stdout}"
    );

    let parsed: Value = serde_json::from_str(&stdout).expect("inputs json");
    assert_eq!(parsed["key"].as_str(), Some(stored_hash.as_str()));
    assert!(
        parsed["salt"]
            .as_str()
            .is_some_and(|salt| salt.starts_with("aster-cache-v")),
        "salt: {}",
        parsed["salt"]
    );
    assert!(
        parsed["platform"]
            .as_str()
            .is_some_and(|platform| platform.starts_with("platform:")),
        "platform: {}",
        parsed["platform"]
    );
    let files = parsed["files"].as_array().expect("files");
    assert!(
        files.iter().any(|file| {
            file["path"] == "package.json" && file["digest"].as_str().is_some_and(|d| !d.is_empty())
        }),
        "files: {files:?}"
    );
    assert_eq!(parsed["command"], "true");
    assert!(parsed["deps"].is_array(), "deps: {}", parsed["deps"]);

    let env = parsed["env"].as_array().expect("env");
    let secret = env
        .iter()
        .find(|var| var["name"] == "APP_SECRET")
        .expect("APP_SECRET");
    let value_hash = secret["value_hash"].as_str().expect("value_hash");
    assert_eq!(value_hash.len(), 64);
    assert_ne!(value_hash, "s3cret-value");
    let unset = env
        .iter()
        .find(|var| var["name"] == "UNSET_CACHE_INPUT")
        .expect("UNSET_CACHE_INPUT");
    assert!(unset["value_hash"].is_null(), "unset: {unset}");

    let runtime = parsed["runtime"].as_array().expect("runtime");
    assert_eq!(runtime.len(), 1, "{runtime:?}");
    assert_eq!(runtime[0]["cmd"], "echo toolchain-1");
    let output_hash = runtime[0]["output_hash"].as_str().expect("output_hash");
    assert_eq!(output_hash.len(), 64);
    assert_ne!(output_hash, "toolchain-1");
    assert!(runtime[0].get("output").is_none());
    assert!(runtime[0].get("error").is_none());
    for field in [
        "key", "salt", "platform", "files", "command", "deps", "env", "runtime",
    ] {
        assert!(parsed.get(field).is_some(), "missing {field}");
    }
}

/// `cache status --inputs` prints the outputs the artifact cache will store:
/// the plugin default when the user omitted the field, and the user's list
/// when they set it.
#[test]
fn cache_status_inputs_shows_effective_outputs() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(
        &tmp,
        "p/package.json",
        "{\"name\":\"p\",\"scripts\":{\"build\":\"mkdir -p dist && echo ok > dist/a.txt\"}}\n",
    );

    let status = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["--json", "cache", "status", "//p:build", "--inputs"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success(),
        "cache status --inputs failed: {} {stdout}",
        String::from_utf8_lossy(&status.stderr)
    );
    let parsed: Value = serde_json::from_str(&stdout).expect("inputs json");
    assert_eq!(parsed["outputs"], serde_json::json!(["dist/**"]));

    write_file(
        &tmp,
        "p/aster.toml",
        "[targets.build]\ncommand = \"npm run build\"\ncache = { outputs = [\"out/**\"] }\n",
    );
    let replaced = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["--json", "cache", "status", "//p:build", "--inputs"])
        .output()
        .unwrap();
    let replaced_stdout = String::from_utf8_lossy(&replaced.stdout);
    assert!(
        replaced.status.success(),
        "replaced status failed: {} {replaced_stdout}",
        String::from_utf8_lossy(&replaced.stderr)
    );
    let replaced_json: Value = serde_json::from_str(&replaced_stdout).expect("inputs json");
    assert_eq!(replaced_json["outputs"], serde_json::json!(["out/**"]));
}

/// `cache status --inputs` recomputes dependency keys from current inputs, so
/// a stale stored dependency entry cannot change the displayed key.
#[test]
fn cache_status_inputs_recomputes_stale_dependency_keys() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "lib/package.json", "{\"name\":\"lib\"}\n");
    write_file(
        &tmp,
        "lib/aster.toml",
        "[targets.build]\ncommand = \"true\"\n\n[targets.build.cache]\nenabled = true\ninclude = [\"package.json\"]\n",
    );
    write_package_json(&tmp, "app/package.json", "{\"name\":\"app\"}\n");
    write_file(
        &tmp,
        "app/aster.toml",
        "[targets.build]\ncommand = \"true\"\ndepends_on = [\"//lib:build\"]\n\n[targets.build.cache]\nenabled = true\ninclude = [\"package.json\"]\n",
    );

    let inputs_key = || {
        let status = assert_cmd::cargo::cargo_bin_cmd!("aster")
            .current_dir(tmp.path())
            .args(["--json", "cache", "status", "//app:build", "--inputs"])
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        let parsed: Value = serde_json::from_slice(&status.stdout).expect("inputs json");
        (
            parsed["key"].as_str().expect("key").to_string(),
            parsed["deps"][0]["hash"].as_str().map(str::to_string),
        )
    };
    let stored_key = |addr: &str| {
        let stored = assert_cmd::cargo::cargo_bin_cmd!("aster")
            .current_dir(tmp.path())
            .args(["--json", "cache", "status", addr])
            .output()
            .unwrap();
        let entry: Value = serde_json::from_slice(&stored.stdout).expect("status json");
        entry["targets"][0]["key"]
            .as_str()
            .expect("stored key")
            .to_string()
    };
    let run = || {
        let run = assert_cmd::cargo::cargo_bin_cmd!("aster")
            .current_dir(tmp.path())
            .args(["run", "//app:build"])
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "run failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    };

    run();
    let (key, dep) = inputs_key();
    assert_eq!(key, stored_key("//app:build"));
    assert_eq!(dep.as_deref(), Some(stored_key("//lib:build").as_str()));

    // Make the stored `//lib:build` entry stale; its fresh key changes.
    write_package_json(&tmp, "lib/package.json", "{\"name\":\"lib\",\"v\":2}\n");
    let stale_lib = stored_key("//lib:build");
    let (key, dep) = inputs_key();
    assert_ne!(dep.as_deref(), Some(stale_lib.as_str()));

    run();
    assert_eq!(key, stored_key("//app:build"));
    assert_eq!(dep.as_deref(), Some(stored_key("//lib:build").as_str()));
}

/// After one successful cached run of a rich target, `cache status --json`
/// lists that artifact. `cache clear //p` then leaves `targets` empty.
#[test]
fn cache_status_json_lists_artifact_then_clear_empties_project() {
    let tmp = TempDir::new().unwrap();
    setup_workspace(&tmp);
    write_package_json(&tmp, "p/package.json", "{\"name\":\"p\"}\n");
    write_file(
        &tmp,
        "p/aster.toml",
        "[targets.build]\ncommand = \"bash -c 'mkdir -p out && echo hi > out/file.txt'\"\n\n[targets.build.cache]\nenabled = true\ninclude = [\"package.json\"]\noutputs = [\"out/file.txt\"]\n",
    );

    let run = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["run", "//p:build", "--no-deps"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "run failed: {} {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    let status = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["cache", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let parsed: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert!(
        parsed["cache_dir"]
            .as_str()
            .is_some_and(|dir| !dir.is_empty()),
        "cache_dir: {}",
        parsed["cache_dir"]
    );
    let targets = parsed["targets"].as_array().expect("targets");
    assert_eq!(targets.len(), 1, "{parsed}");
    let row = &targets[0];
    assert_eq!(row["address"], "//p:build");
    let key = row["key"].as_str().expect("key");
    assert!(
        key.len() == 64 && key.chars().all(|ch| ch.is_ascii_hexdigit()),
        "key: {key}"
    );
    assert_eq!(row["outputs"], 1);
    assert!(row["output_bytes"].as_u64().is_some());
    assert_eq!(row["exit_code"], 0);
    assert!(
        row["created_at"]
            .as_str()
            .is_some_and(|stamp| stamp.contains('T')),
        "created_at: {}",
        row["created_at"]
    );

    let human = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["cache", "status"])
        .output()
        .unwrap();
    let human_out = String::from_utf8_lossy(&human.stdout);
    assert!(human.status.success(), "{human_out}");
    assert!(human_out.contains("//p:build"), "{human_out}");
    assert!(human_out.contains("OUTPUTS"), "{human_out}");

    let clear = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["cache", "clear", "//p"])
        .output()
        .unwrap();
    assert!(
        clear.status.success(),
        "clear failed: {}",
        String::from_utf8_lossy(&clear.stderr)
    );

    let after = assert_cmd::cargo::cargo_bin_cmd!("aster")
        .current_dir(tmp.path())
        .args(["cache", "status", "--json"])
        .output()
        .unwrap();
    let after_json: Value = serde_json::from_slice(&after.stdout).expect("status json");
    assert!(
        after_json["targets"]
            .as_array()
            .is_some_and(|rows| rows.is_empty()),
        "targets should be empty after clear //p: {after_json}"
    );
}
