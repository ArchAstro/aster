//! Real Cargo workspace ownership through the public CLI.
//!
//! Integration matrix: root and member entry points, several selected members,
//! affected-file selection, cold/warm cache, changed unselected member inputs,
//! newly discovered members, aliases, and a compiler failure. Virtual and
//! package roots, nested workspaces, excluded crates, external source paths,
//! custom output directories, and personal ignore rules exercise input ownership.
//! General discovery/planning and independent non-Rust work run with Cargo absent
//! or guarded; mixed-language prerequisites succeed/fail at real Rust execution. Package tests and build
//! variants remain separate. Dependency-only targets also exercise prerequisite
//! failure, sparse missing prerequisites, and incompatible execution flags.
//! Fidelity: production Aster discovery/scheduler/cache, Cargo compiler, and Git.
//! Cargo-free discovery uses an absent tool and an invocation-recording guard. Shell wrappers only record which real Cargo gates ran.
//! Automatic coverage: CI pull requests/main pushes run the existing integration
//! test binary on Linux and macOS. These Unix fixtures do not cover Windows.
#![cfg(unix)]

use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

use super::{setup_workspace, write_file, write_package_json};

fn aster(repo: &TempDir, runtime: &TempDir, args: &[&str]) -> Output {
    // A developer's global ignores must neither leak into the fixture nor hide
    // authored build inputs. The deliberately ignored text assets still count.
    let ignores = runtime.path().join("global.ignore");
    let gitconfig = runtime.path().join("gitconfig");
    fs::write(&ignores, "*.txt\n").unwrap();
    fs::write(&gitconfig, format!("[core]\nexcludesFile = {ignores:?}\n")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(repo.path())
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("ASTER_EXECUTION_DIR", runtime.path().join("host"))
        .env("ASTER_CACHE_DIR", runtime.path().join("cache"))
        .env("ASTER_JOBS", "4")
        .env("WORKSPACE_GATE_LOG", runtime.path().join("gates.log"))
        .env("WORKSPACE_TEST_LOG", runtime.path().join("tests.log"))
        .env_remove("ASTER_EXECUTION_CONTEXT")
        .env_remove("ASTER_REMOTE_CACHE")
        .env("ASTER_CACHE_MODE", "off")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .args(args)
        .output()
        .unwrap()
}

fn successful(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn gate_count(runtime: &TempDir, name: &str) -> usize {
    fs::read_to_string(runtime.path().join("gates.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == name)
        .count()
}

fn cargo_lockfile(repo: &TempDir) {
    successful(
        Command::new("cargo")
            .current_dir(repo.path())
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .args(["generate-lockfile", "--offline"])
            .output()
            .unwrap(),
    );
}

fn git(repo: &TempDir, args: &[&str]) {
    successful(
        Command::new("git")
            .current_dir(repo.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap(),
    );
}

#[test]
fn configured_cargo_workspace_discovery_works_without_cargo() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    // The configured Rust owner is present even when the selected work is web-only.
    // An inherited, target-specific dependency also contributes an implicit member.
    write_file(&repo, "Cargo.toml", "[workspace]\nmembers = [\"crates/a*\"]\n[workspace.dependencies]\nshared = { package = \"basis\", path = \"crates/basis\" }\n");
    write_file(&repo, "aster.toml", "[cargo.workspace]\ntargets = [\"build\"]\n[targets.build]\ncommand = \"cargo build --workspace --offline\"\n");
    write_file(&repo, "crates/app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[target.'cfg(unix)'.dependencies]\nshared.workspace = true\n");
    write_file(
        &repo,
        "crates/basis/Cargo.toml",
        "[package]\nname = \"basis\"\nversion = \"0.1.0\"\n",
    );
    for member in ["app", "basis"] {
        write_file(
            &repo,
            &format!("crates/{member}/src/lib.rs"),
            "pub fn value() {}\n",
        );
    }
    write_package_json(&repo, "web/package.json", r#"{"name":"web"}"#);
    write_file(&repo, "web/aster.toml", "[targets.verify]\ncommand = \"/bin/echo web-ok\"\ncache = { runtime = [] }\n[targets.bundle]\ncommand = \"/bin/echo bundle-ok\"\ndepends_on = [\"//crates/app:build\"]\ncache = { runtime = [] }\n");
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "mixed workspace",
        ],
    );
    // A nested Git checkout is still a member of its declared Cargo workspace.
    fs::create_dir(repo.path().join("crates/app/.git")).unwrap();
    let manifests: Vec<_> = [
        "Cargo.toml",
        "crates/app/Cargo.toml",
        "crates/basis/Cargo.toml",
    ]
    .into_iter()
    .map(|path| (path, fs::read(repo.path().join(path)).unwrap()))
    .collect();
    let bin = runtime.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Keep Git available for affected planning, but omit Cargo and rustup.
    let git_binary = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    std::os::unix::fs::symlink(git_binary, bin.join("git")).unwrap();
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .current_dir(repo.path())
            .env("PATH", &bin)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("ASTER_EXECUTION_DIR", runtime.path().join("host"))
            .env("ASTER_CACHE_DIR", runtime.path().join("cache"))
            .env_remove("ASTER_EXECUTION_CONTEXT")
            .env_remove("ASTER_REMOTE_CACHE")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .args(args)
            .output()
            .unwrap()
    };

    // Cross the public CLI/process boundary with no Cargo (or rustup shim) on PATH.
    let listing = successful(invoke(&["--json", "list"]));
    let projects: Value = serde_json::from_slice(&listing.stdout).unwrap();
    for address in ["//", "//crates/app", "//crates/basis", "//web"] {
        assert!(
            projects
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["address"] == address),
            "missing {address}: {projects}"
        );
    }
    let graph_output = successful(invoke(&["--json", "graph"]));
    let graph: Value = serde_json::from_slice(&graph_output.stdout).unwrap();
    let available = successful(aster(&repo, &runtime, &["--json", "graph"]));
    assert_eq!(
        serde_json::from_slice::<Value>(&available.stdout).unwrap(),
        graph
    );
    for member in ["app", "basis"] {
        assert_eq!(
            graph["edges"][format!("//crates/{member}:build")],
            serde_json::json!(["//:build"])
        );
    }
    assert!(graph["edges"]["//web:bundle"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("//crates/app:build")));
    successful(invoke(&["check"]));
    let web = successful(invoke(&["run", "//web:verify", "--json"]));
    let web: Value = serde_json::from_slice(&web.stdout).unwrap();
    assert_eq!(web["results"][0]["address"], "//web:verify");
    assert_eq!(web["results"][0]["output"], "web-ok\n");

    // Planning still selects the workspace owner after an implicit member changes.
    write_file(&repo, "crates/basis/src/lib.rs", "pub fn changed() {}\n");
    let plan = successful(invoke(&[
        "affected",
        "build",
        "--base=HEAD",
        "--dry-run",
        "--json",
    ]));
    assert!(String::from_utf8_lossy(&plan.stdout).contains("//:build"));

    // An installed but unusable Cargo must not be probed or have its failure swallowed.
    let guard = bin.join("cargo");
    fs::write(
        &guard,
        format!(
            "#!/bin/sh\nprintf invoked >> '{}'\nexit 97\n",
            runtime.path().join("cargo-calls").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&guard, fs::Permissions::from_mode(0o755)).unwrap();
    let guarded_graph = successful(invoke(&["--json", "graph"]));
    assert_eq!(
        serde_json::from_slice::<Value>(&guarded_graph.stdout).unwrap(),
        graph
    );
    successful(invoke(&["check"]));
    successful(invoke(&["list"]));
    successful(invoke(&[
        "affected",
        "build",
        "--base=HEAD",
        "--dry-run",
        "--json",
    ]));
    successful(invoke(&["run", "//web:verify", "--json"]));
    assert!(!runtime.path().join("cargo-calls").exists());
    for (path, original) in manifests {
        assert_eq!(
            fs::read(repo.path().join(path)).unwrap(),
            original,
            "discovery changed {path}"
        );
    }
    assert!(!repo.path().join("Cargo.lock").exists());

    // Real Rust prerequisites retain their execution boundary and report a missing tool.
    fs::remove_file(guard).unwrap();
    for target in ["//:build", "//web:bundle"] {
        let output = invoke(&["run", target]);
        assert!(!output.status.success(), "Rust work unexpectedly succeeded");
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(diagnostic.contains("cargo"), "{diagnostic}");
        assert!(
            !diagnostic.contains("Discover Cargo workspace ownership"),
            "{diagnostic}"
        );
    }
}

#[test]
fn cargo_workspace_membership_respects_excluded_directories_and_nested_checkouts() {
    // Cargo's member globs and exclusion directory prefixes are different rules.
    // Confirm the fixture with real Cargo, then inspect production Aster forwarding.
    for (exclude, owns_basis) in [
        ("vendor", false),
        ("vendor/basis", false),
        ("vendor/*", true),
    ] {
        let repo = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        setup_workspace(&repo);
        write_file(
            &repo,
            "Cargo.toml",
            &format!("[workspace]\nmembers = [\"app\"]\nexclude = [\"{exclude}\"]\n"),
        );
        write_file(&repo, "aster.toml", "[cargo.workspace]\ntargets = [\"build\"]\n[targets.build]\ncommand = \"cargo build --workspace --offline\"\n");
        write_file(&repo, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\nbasis = { path = \"../vendor/basis\" }\n");
        write_file(&repo, "app/src/lib.rs", "pub fn app() {}\n");
        write_file(
            &repo,
            "vendor/basis/Cargo.toml",
            "[package]\nname = \"basis\"\nversion = \"0.1.0\"\n",
        );
        write_file(&repo, "vendor/basis/src/lib.rs", "pub fn basis() {}\n");
        fs::create_dir(repo.path().join("app/.git")).unwrap();
        let metadata = successful(
            Command::new("cargo")
                .current_dir(repo.path())
                .args([
                    "metadata",
                    "--no-deps",
                    "--offline",
                    "--format-version",
                    "1",
                ])
                .output()
                .unwrap(),
        );
        let metadata: Value = serde_json::from_slice(&metadata.stdout).unwrap();
        assert_eq!(
            metadata["workspace_members"].as_array().unwrap().len(),
            if owns_basis { 2 } else { 1 }
        );
        let graph = successful(aster(&repo, &runtime, &["graph", "--json"]));
        let graph: Value = serde_json::from_slice(&graph.stdout).unwrap();
        assert_eq!(
            graph["edges"]["//app:build"],
            serde_json::json!(["//:build"])
        );
        let basis = &graph["edges"]["//vendor/basis:build"];
        assert_eq!(
            basis
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("//:build")),
            owns_basis,
            "wrong ownership with exclude={exclude}: {graph}"
        );
    }
}

#[test]
fn cargo_workspace_members_share_gates_and_invalidate_unselected_inputs() {
    let repo = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();

    // Local manifests define membership: app matches the glob, basis joins as
    // its path dependency. The virtual root has no package to build itself.
    write_file(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a*\"]\nresolver = \"2\"\n[workspace.dependencies]\napp-self = { package = \"app\", path = \"crates/app\" }\n",
    );
    write_file(&repo, ".gitignore", "target/\n.aster/\n");
    write_file(
        &repo,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nbasis = { path = \"../basis\" }\n[dev-dependencies]\napp-self.workspace = true\n",
    );
    write_file(
        &repo,
        "crates/basis/Cargo.toml",
        "[package]\nname = \"basis\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(
        &repo,
        "crates/basis/src/lib.rs",
        "pub fn answer() -> usize { include_str!(\"message.txt\").len() }\n#[test] fn basis_only() { std::fs::write(std::path::PathBuf::from(std::env::var(\"WORKSPACE_TEST_LOG\").unwrap()).with_extension(\"basis\"), \"basis\").unwrap(); }\n",
    );
    write_file(&repo, "crates/basis/src/message.txt", "hello\n");
    write_file(
        &repo,
        "crates/app/src/lib.rs",
        "pub fn answer() -> usize { basis::answer() }\n#[test] fn app_only() { assert!(answer() > 0); std::fs::write(std::path::PathBuf::from(std::env::var(\"WORKSPACE_TEST_LOG\").unwrap()).with_extension(\"app\"), \"app\").unwrap(); }\n",
    );
    write_file(
        &repo,
        "aster.toml",
        r#"
[cargo.workspace]
targets = ["deps", "build", "lint"]

[targets.deps]
command = "sh -c 'echo deps >> \"$WORKSPACE_GATE_LOG\"; cargo fetch --offline'"

[targets.build]
command = "sh -c 'echo build >> \"$WORKSPACE_GATE_LOG\"; cargo build --workspace --offline'"
depends_on = ["//self:deps"]

[targets.lint]
command = "sh -c 'echo lint >> \"$WORKSPACE_GATE_LOG\"; cargo check --workspace --offline'"
depends_on = ["//self:deps"]

[targets.check]
alias = "build"
"#,
    );
    write_file(
        &repo,
        "crates/app/aster.toml",
        r#"
[targets.test]
command = "sh -c 'echo package-test >> \"$WORKSPACE_GATE_LOG\"; cargo test -p app --offline'"

[targets.build-release]
command = "sh -c 'echo release >> \"$WORKSPACE_GATE_LOG\"; cargo build -p app --release --offline'"

[targets.verify]
alias = "build"
"#,
    );
    // Put real Cargo artifacts inside a member: exclusion must keep them out
    // of the owner's input hash even without an ignore rule for this directory.
    write_file(
        &repo,
        ".cargo/config.toml",
        "[build]\ntarget-dir = \"crates/app/output\"\n",
    );
    write_package_json(&repo, "web/package.json", r#"{"name":"web"}"#);
    write_file(
        &repo,
        "web/aster.toml",
        "[targets.bundle]\ncommand = \"/bin/echo bundled\"\ndepends_on = [\"//crates/app:build\"]\n",
    );
    cargo_lockfile(&repo);
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "workspace fixture",
        ],
    );

    // A non-Rust entry point must wait for its real Rust prerequisite.
    let bundle = successful(aster(&repo, &runtime, &["run", "//web:bundle", "--json"]));
    let bundle: Value = serde_json::from_slice(&bundle.stdout).unwrap();
    assert!(bundle["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["address"] == "//web:bundle" && result["output"] == "bundled\n"));
    assert!(repo
        .path()
        .join("crates/app/output/debug/libapp.rlib")
        .is_file());

    // Root and both member addresses cross the real scheduler/Cargo boundary.
    // Each workspace gate must run exactly once despite the overlapping entry points.
    for target in ["build", "lint"] {
        successful(aster(
            &repo,
            &runtime,
            &[target, "//", "//crates/app", "//crates/basis", "--json"],
        ));
    }
    for gate in ["deps", "build", "lint"] {
        assert_eq!(gate_count(&runtime, gate), 1, "duplicate {gate} gate");
    }
    assert!(repo
        .path()
        .join("crates/app/output/debug/libapp.rlib")
        .is_file());

    // A second invocation must reuse the owner's result; member entry points
    // must not invent separately cached successes or launch another Cargo command.
    for target in ["build", "lint"] {
        let output = successful(aster(
            &repo,
            &runtime,
            &[target, "//", "//crates/app", "//crates/basis", "--json"],
        ));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let owner = format!("//:{target}");
        assert!(value["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| { result["address"] == owner && result["cached"] == true }));
        for result in value["results"].as_array().unwrap() {
            if result["address"].as_str().unwrap().starts_with("//crates/") {
                assert_eq!(
                    result["cached"], false,
                    "member cached separately: {result}"
                );
            }
        }
    }
    for gate in ["deps", "build", "lint"] {
        assert_eq!(gate_count(&runtime, gate), 1, "warm {gate} ran again");
    }

    // Select app alone after changing basis. The workspace owner's inputs must
    // include an unselected member even though ordinary project hashes exclude it.
    let basis = repo.path().join("crates/basis/src/lib.rs");
    let source = fs::read_to_string(&basis).unwrap();
    fs::write(
        &basis,
        format!("{source}\npub const REVISION: usize = 2;\n"),
    )
    .unwrap();
    successful(aster(&repo, &runtime, &["run", "//crates/app:build"]));
    assert_eq!(gate_count(&runtime, "build"), 2);
    assert_eq!(gate_count(&runtime, "deps"), 1);

    // Embedded non-Rust inputs also invalidate the owner. Use the affected
    // entry point to prove selection and caching agree about the workspace scope.
    write_file(&repo, "crates/basis/src/message.txt", "changed payload\n");
    successful(aster(
        &repo,
        &runtime,
        &["affected", "build", "--base=HEAD", "--only-affected-files"],
    ));
    assert_eq!(gate_count(&runtime, "build"), 3);

    // A newly added matching crate needs no aster.toml or hand-maintained list.
    write_file(
        &repo,
        "crates/additional/Cargo.toml",
        "[package]\nname = \"additional\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(
        &repo,
        "crates/additional/src/lib.rs",
        "pub fn new_member() {}\n",
    );
    cargo_lockfile(&repo);
    successful(aster(
        &repo,
        &runtime,
        &["run", "//crates/additional:build"],
    ));
    assert_eq!(gate_count(&runtime, "build"), 4);
    assert!(repo
        .path()
        .join("crates/app/output/debug/libadditional.rlib")
        .is_file());
    assert!(!repo.path().join("crates/additional/aster.toml").exists());

    // A member alias must resolve to the generated commandless entry, while a
    // root alias retains command-copy semantics and the owner's complete inputs.
    let output = successful(aster(
        &repo,
        &runtime,
        &["run", "//crates/app:verify", "--json"],
    ));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["results"].as_array().unwrap().iter().any(|result| {
            result["address"] == "//crates/app:verify"
                && result["exit_code"].is_null()
                && result["cached"] == false
        }),
        "member alias launched a command: {value}"
    );
    assert_eq!(gate_count(&runtime, "build"), 4);
    successful(aster(&repo, &runtime, &["run", "//:check"]));
    assert_eq!(gate_count(&runtime, "build"), 5);
    successful(aster(&repo, &runtime, &["run", "//:check"]));
    assert_eq!(gate_count(&runtime, "build"), 5);
    write_file(
        &repo,
        "crates/basis/src/message.txt",
        "alias observes this payload\n",
    );
    successful(aster(&repo, &runtime, &["run", "//:check"]));
    assert_eq!(gate_count(&runtime, "build"), 6);

    // Ownership is explicit: package tests and differently named configurations
    // keep their independent commands, without a redundant path-dependency build.
    successful(aster(&repo, &runtime, &["run", "//crates/app:test"]));
    assert_eq!(
        fs::read_to_string(runtime.path().join("tests.app")).unwrap(),
        "app"
    );
    assert!(
        !runtime.path().join("tests.basis").exists(),
        "package selection ran basis tests"
    );
    successful(aster(
        &repo,
        &runtime,
        &["run", "//crates/app:build-release"],
    ));
    assert_eq!(gate_count(&runtime, "package-test"), 1);
    assert_eq!(gate_count(&runtime, "release"), 1);
    assert_eq!(gate_count(&runtime, "build"), 6);

    // An API break in the dependency must fail the workspace compilation.
    // The member entry does not run after that gate fails, so its public
    // result is blocked rather than a failure or a stale successful forward.
    fs::write(&basis, "pub fn renamed_answer() -> usize { 1 }\n").unwrap();
    let output = aster(&repo, &runtime, &["run", "//crates/app:build", "--json"]);
    assert!(!output.status.success(), "broken workspace passed");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = value["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .any(|result| { result["address"] == "//:build" && result["status"] == "failed" }),
        "missing compilation failure for //:build: {value}"
    );
    assert!(
        results.iter().any(|result| {
            result["address"] == "//crates/app:build"
                && result["status"] == "blocked"
                && result["cached"] == false
                && result["exit_code"].is_null()
                && result["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("//:build"))
        }),
        "member result should be blocked by the failed workspace build, not a success: {value}"
    );
    assert_eq!(value["summary"]["failed"], 1);
    assert_eq!(value["summary"]["blocked"], 1);
    assert!(String::from_utf8_lossy(&output.stdout).contains("cannot find function"));
    assert_eq!(gate_count(&runtime, "build"), 7);
}

#[test]
fn nested_cargo_workspace_hashes_external_modules_without_claiming_excluded_crates() {
    let repo = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    setup_workspace(&repo);
    write_file(&repo, ".gitignore", "target/\n.aster/\n");
    // This nested workspace has a root package. Its member's lib and sibling
    // module/assets live outside both the package and Cargo workspace directories.
    write_file(&repo, "rust/Cargo.toml", "[package]\nname = \"root_package\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\nmembers = [\"members/app\", \"../external\"]\nexclude = [\"excluded\"]\nresolver = \"2\"\n");
    write_file(
        &repo,
        "external/Cargo.toml",
        "[package]\nname = \"external_member\"\nversion = \"0.1.0\"\nworkspace = \"../rust\"\n",
    );
    write_file(
        &repo,
        "external/src/lib.rs",
        "pub fn external_member() {}\n",
    );
    write_file(&repo, "rust/src/lib.rs", "pub fn root_package() {}\n");
    write_file(&repo, "rust/members/app/Cargo.toml", "[package]\nname = \"external_source\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"../../../shared/lib.rs\"\n");
    write_file(
        &repo,
        "shared/lib.rs",
        "mod helper;\npub fn value() -> usize { helper::value() }\n",
    );
    write_file(
        &repo,
        "shared/helper.rs",
        "pub fn value() -> usize { include_str!(\"payload.txt\").len() }\n",
    );
    write_file(&repo, "shared/payload.txt", "outside workspace\n");
    write_file(
        &repo,
        "rust/excluded/Cargo.toml",
        "[package]\nname = \"excluded\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&repo, "rust/excluded/src/lib.rs", "pub fn excluded() {}\n");
    write_file(
        &repo,
        "rust/aster.toml",
        r#"
[cargo.workspace]
targets = ["build"]
[targets.build]
command = "sh -c 'echo nested >> \"$WORKSPACE_GATE_LOG\"; cargo build --workspace --offline'"
"#,
    );
    write_file(
        &repo,
        "rust/excluded/aster.toml",
        r#"
[targets.build]
command = "sh -c 'echo excluded >> \"$WORKSPACE_GATE_LOG\"; cargo build --offline'"
"#,
    );
    successful(
        Command::new("cargo")
            .current_dir(repo.path().join("rust"))
            .args(["generate-lockfile", "--offline"])
            .output()
            .unwrap(),
    );

    successful(aster(
        &repo,
        &runtime,
        &["build", "//rust", "//rust/members/app", "//external"],
    ));
    assert_eq!(gate_count(&runtime, "nested"), 1);
    assert!(repo
        .path()
        .join("rust/target/debug/libexternal_source.rlib")
        .is_file());

    // Changes in an excluded independent package must not invalidate or invoke
    // the workspace owner, and its own target must remain executable.
    write_file(
        &repo,
        "rust/excluded/src/lib.rs",
        "pub fn excluded_changed() {}\n",
    );
    successful(aster(&repo, &runtime, &["run", "//rust/members/app:build"]));
    assert_eq!(gate_count(&runtime, "nested"), 1);
    successful(aster(&repo, &runtime, &["run", "//rust/excluded:build"]));
    assert_eq!(gate_count(&runtime, "excluded"), 1);
    assert_eq!(gate_count(&runtime, "nested"), 1);

    // Commit all setup and excluded-crate changes. The next change will be
    // outside every discovered project, so ordinary member selection cannot
    // hide a broken workspace-input selector by forwarding to the owner.
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "external input baseline",
        ],
    );

    // The manifest names lib.rs; changing its sibling mod alone must select
    // and invalidate the owner through the affected entry point.
    write_file(
        &repo,
        "shared/helper.rs",
        "pub fn value() -> usize { include_str!(\"payload.txt\").len() + 1 }\n",
    );
    successful(aster(
        &repo,
        &runtime,
        &["affected", "build", "--base=HEAD", "--only-affected-files"],
    ));
    assert_eq!(gate_count(&runtime, "nested"), 2);
    // The sibling embedded asset is still an input despite the personal *.txt
    // ignore configured by the CLI helper.
    write_file(&repo, "shared/payload.txt", "changed outside workspace\n");
    successful(aster(&repo, &runtime, &["run", "//rust/members/app:build"]));
    assert_eq!(gate_count(&runtime, "nested"), 3);

    // Isolate deletion of the asset as the only Git change. It must still select
    // the owner and surface the real compiler error instead of returning no work.
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "external input update",
        ],
    );
    fs::remove_file(repo.path().join("shared/payload.txt")).unwrap();
    let output = aster(
        &repo,
        &runtime,
        &[
            "affected",
            "build",
            "--base=HEAD",
            "--only-affected-files",
            "--json",
        ],
    );
    assert!(
        !output.status.success(),
        "deleted external input was not checked"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| { result["address"] == "//rust:build" && result["status"] == "failed" }),
        "workspace owner did not fail: {value}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("payload.txt"));
    assert_eq!(gate_count(&runtime, "nested"), 4);
}

#[test]
fn unconfigured_cargo_workspaces_keep_package_targets_without_invoking_cargo() {
    let repo = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    setup_workspace(&repo);
    write_file(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers = [\"members/*\"]\nresolver = \"2\"\n",
    );
    write_file(&repo, "members/app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\ncore = { path = \"../core\" }\n");
    write_file(
        &repo,
        "members/core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
    );
    for member in ["app", "core"] {
        write_file(
            &repo,
            &format!("members/{member}/src/lib.rs"),
            "pub fn library() {}\n",
        );
    }

    // There is deliberately no ownership declaration and no Cargo executable
    // on PATH. Ordinary discovery must remain usable without running metadata.
    let empty_bin = runtime.path().join("empty-bin");
    fs::create_dir(&empty_bin).unwrap();
    let gitconfig = runtime.path().join("gitconfig");
    fs::write(&gitconfig, "").unwrap();
    let inspect = |command| {
        let output = successful(
            Command::new(env!("CARGO_BIN_EXE_aster"))
                .current_dir(repo.path())
                .env("PATH", &empty_bin)
                .env("GIT_CONFIG_GLOBAL", &gitconfig)
                .args(["--json", command])
                .output()
                .unwrap(),
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let projects = inspect("list");
    let projects = projects.as_array().unwrap();
    assert_eq!(
        projects.len(),
        2,
        "unconfigured virtual root became a project"
    );
    for member in ["app", "core"] {
        let address = format!("//members/{member}");
        let project = projects
            .iter()
            .find(|project| project["address"] == address)
            .unwrap();
        for target in ["build", "check", "test"] {
            assert_eq!(project["targets"][target], format!("cargo {target}"));
        }
    }

    // Existing package execution dependencies are unchanged until opt-in:
    // app still builds its path dependency before build/check/test.
    let graph = inspect("graph");
    for target in ["build", "check", "test"] {
        let mut edges: Vec<_> = graph["edges"][format!("//members/app:{target}")]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        edges.sort();
        assert_eq!(edges, ["//members/app:deps", "//members/core:build"]);
        assert_eq!(
            graph["edges"][format!("//members/core:{target}")],
            serde_json::json!(["//members/core:deps"])
        );
    }
    assert!(!graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node.as_str().unwrap().starts_with("//:")));
}

#[test]
fn cargo_workspace_rejects_ambiguous_or_cyclic_ownership_before_execution() {
    for (name, root_config, member_config, expected) in [
        (
            "member override",
            "[cargo.workspace]\ntargets = [\"build\"]\n[targets.build]\ncommand = \"touch executed\"\n",
            "[targets.build]\ncommand = \"echo duplicate\"\n",
            vec!["//crates/app:build", "//:build"],
        ),
        (
            "undefined owner",
            "[cargo.workspace]\ntargets = [\"build\"]\n",
            "",
            vec!["build", "explicit"],
        ),
        (
            "duplicate name",
            "[cargo.workspace]\ntargets = [\"build\", \"build\"]\n[targets.build]\ncommand = \"touch executed\"\n",
            "",
            vec!["duplicate", "build"],
        ),
        (
            "non-root declaration",
            "[cargo.workspace]\ntargets = [\"build\"]\n[targets.build]\ncommand = \"touch executed\"\n",
            "[cargo.workspace]\ntargets = [\"custom\"]\n[targets.custom]\ncommand = \"echo member\"\n",
            vec!["workspace root", "crates/app"],
        ),
        (
            "owner depends on its forward",
            "[cargo.workspace]\ntargets = [\"build\"]\n[targets.build]\ncommand = \"touch executed\"\ndepends_on = [\"//crates/app:build\"]\n",
            "",
            vec!["cycle", "build"],
        ),
    ] {
        let repo = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        setup_workspace(&repo);
        write_file(&repo, "Cargo.toml", "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n");
        write_file(&repo, "crates/app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write_file(&repo, "crates/app/src/lib.rs", "pub fn app() {}\n");
        write_file(&repo, "aster.toml", root_config);
        write_file(&repo, "crates/app/aster.toml", member_config);
        let output = aster(&repo, &runtime, &["run", "//:build"]);
        assert!(!output.status.success(), "{name} unexpectedly succeeded");
        let diagnostic = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        for fragment in expected {
            assert!(diagnostic.to_lowercase().contains(&fragment.to_lowercase()), "{name}: missing {fragment:?} in {diagnostic}");
        }
        assert!(!repo.path().join("executed").exists(), "{name} executed its owner before rejection");
    }

    // Invalid opt-in configuration at a nested virtual root must not disappear
    // when discovery skips ordinary virtual roots and visits their members.
    let repo = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    setup_workspace(&repo);
    write_file(
        &repo,
        "rust/Cargo.toml",
        "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n",
    );
    write_file(
        &repo,
        "rust/member/Cargo.toml",
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
    );
    write_file(&repo, "rust/member/src/lib.rs", "pub fn member() {}\n");
    write_file(
        &repo,
        "rust/aster.toml",
        "[cargo.workspace]\ntargets = \"build\"\n",
    );
    write_file(
        &repo,
        "rust/member/aster.toml",
        "[targets.work]\ncommand = \"touch executed\"\n",
    );
    let output = aster(&repo, &runtime, &["run", "//rust/member:work"]);
    assert!(
        !output.status.success(),
        "invalid virtual-root configuration was ignored"
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("rust/aster.toml") && diagnostic.contains("invalid type"),
        "{diagnostic}"
    );
    assert!(!repo.path().join("rust/member/executed").exists());
}

#[test]
fn dependency_only_targets_follow_prerequisite_results_without_own_cache_entries() {
    let repo = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    setup_workspace(&repo);
    for project in ["owner", "entry", "consumer"] {
        write_package_json(
            &repo,
            &format!("{project}/package.json"),
            &format!(r#"{{"name":"{project}"}}"#),
        );
    }
    write_file(
        &repo,
        "owner/aster.toml",
        r#"
[targets.work]
command = "sh -c 'echo owner >> \"$WORKSPACE_GATE_LOG\"'"
cache = { enabled = true, runtime = [] }
"#,
    );
    write_file(
        &repo,
        "entry/aster.toml",
        "[targets.work]\ndepends_on = [\"//owner:work\"]\n",
    );
    write_file(
        &repo,
        "consumer/aster.toml",
        r#"
[targets.work]
command = "sh -c 'echo consumer >> \"$WORKSPACE_GATE_LOG\"'"
depends_on = ["//entry:work"]
cache = { enabled = true, runtime = [] }
"#,
    );

    // A commandless entry joins the real owner to a downstream consumer. Both
    // commands can be cached, while the entry is reevaluated from prerequisites.
    successful(aster(&repo, &runtime, &["run", "//consumer:work"]));
    let output = successful(aster(
        &repo,
        &runtime,
        &["run", "//consumer:work", "--json"],
    ));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(gate_count(&runtime, "owner"), 1);
    assert_eq!(gate_count(&runtime, "consumer"), 1);
    for (address, cached) in [
        ("//owner:work", true),
        ("//entry:work", false),
        ("//consumer:work", true),
    ] {
        assert!(
            value["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|result| result["address"] == address && result["cached"] == cached),
            "{value}"
        );
    }
    let output = successful(aster(
        &repo,
        &runtime,
        &["--json", "cache", "status", "//entry:work"],
    ));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value.is_null(),
        "commandless entry stored an artifact result: {value}"
    );

    // Omitting the only executable prerequisite or forcing direct streaming
    // must not turn a dependency-only request into a successful no-op.
    for flag in ["--no-deps", "--stream"] {
        let output = aster(&repo, &runtime, &["work", "//entry", flag, "--json"]);
        assert!(
            !output.status.success(),
            "{flag} reported success without work"
        );
        let diagnostic = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("dependency") || diagnostic.contains("prerequisite"),
            "{diagnostic}"
        );
    }
    assert_eq!(gate_count(&runtime, "owner"), 1);

    // The entry's dependency hash must carry an owner's changed inputs through
    // to the consumer, without storing an independent success for the entry.
    write_file(
        &repo,
        "owner/aster.toml",
        r#"
[targets.work]
command = "sh -c 'echo owner >> \"$WORKSPACE_GATE_LOG\"; echo revised'"
cache = { enabled = true, runtime = [] }
"#,
    );
    successful(aster(&repo, &runtime, &["run", "//consumer:work"]));
    assert_eq!(gate_count(&runtime, "owner"), 2);
    assert_eq!(gate_count(&runtime, "consumer"), 2);

    // Replacing the prerequisite with a failing command must invalidate its
    // old cache entry. The commandless entry and its consumer never run, so
    // they are blocked by that failure rather than counted as failures.
    write_file(
        &repo,
        "owner/aster.toml",
        r#"
[targets.work]
command = "sh -c 'echo owner >> \"$WORKSPACE_GATE_LOG\"; exit 7'"
cache = { enabled = true, runtime = [] }
"#,
    );
    let output = aster(&repo, &runtime, &["run", "//consumer:work", "--json"]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = value["results"].as_array().unwrap();
    assert!(
        results.iter().any(|result| {
            result["address"] == "//owner:work"
                && result["status"] == "failed"
                && result["exit_code"] == 7
                && result["cached"] == false
        }),
        "owner failure did not replace the cached success: {value}"
    );
    assert!(
        results.iter().any(|result| {
            result["address"] == "//entry:work"
                && result["status"] == "blocked"
                && result["cached"] == false
                && result["exit_code"].is_null()
                && result["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("//owner:work"))
        }),
        "commandless entry should be blocked by the failed prerequisite, not cached or passed: {value}"
    );
    assert!(
        results.iter().any(|result| {
            result["address"] == "//consumer:work"
                && result["status"] == "blocked"
                && result["cached"] == false
                && result["exit_code"].is_null()
        }),
        "consumer should not run or hit cache after the prerequisite failed: {value}"
    );
    assert_eq!(value["summary"]["failed"], 1);
    assert_eq!(value["summary"]["blocked"], 2);
    assert_eq!(gate_count(&runtime, "owner"), 3);
    assert_eq!(gate_count(&runtime, "consumer"), 2);

    // Sparse discovery may omit ordinary unavailable dependency projects, but
    // omitting a commandless entry's only operation cannot become success.
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "core.sparseCheckout", "true"]);
    write_file(
        &repo,
        "entry/aster.toml",
        "[targets.work]\ndepends_on = [\"//missing:work\"]\n",
    );
    let output = aster(&repo, &runtime, &["run", "//entry:work"]);
    assert!(
        !output.status.success(),
        "sparse checkout discarded the required operation"
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("//missing:work"), "{diagnostic}");
    assert_eq!(gate_count(&runtime, "owner"), 3);
    assert_eq!(gate_count(&runtime, "consumer"), 2);
}
