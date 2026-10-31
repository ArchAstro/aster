//! Documented `cargo install --git` commands must name a workspace binary.
//!
//! Without a package name, that command refuses a workspace that contains both
//! `aster` and `aster-srv`.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

const DOCS: &[&str] = &["README.md", "GETTING_STARTED.MD"];

/// Flags that consume the following token. `--flag=value` does not.
const VALUE_FLAGS: &[&str] = &[
    "--bin",
    "--branch",
    "--color",
    "--config",
    "--example",
    "--features",
    "--git",
    "--index",
    "--jobs",
    "--manifest-path",
    "--message-format",
    "--path",
    "--profile",
    "--registry",
    "--rev",
    "--root",
    "--tag",
    "--target",
    "--version",
    "-Z",
    "-j",
];

#[test]
fn documented_cargo_install_commands_name_a_package() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let binaries = workspace_binary_packages(root);

    for relative in DOCS {
        let path = root.join(relative);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let lines = cargo_install_git_lines(&text);
        assert!(
            !lines.is_empty(),
            "{relative} has no `cargo install --git` command"
        );
        for line in lines {
            let packages = package_names(&line);
            assert!(
                !packages.is_empty(),
                "{relative} `cargo install --git` line names no package: {line}"
            );
            let last = line.split_whitespace().next_back().unwrap_or("");
            assert_eq!(
                last, "aster",
                "{relative} install line must end with the package name `aster`: {line}"
            );
            for package in &packages {
                assert!(
                    binaries.iter().any(|name| name == package),
                    "{package} in {relative} is not a workspace binary package; binaries: {binaries:?}"
                );
            }
        }
    }
}

fn cargo_install_git_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.contains("cargo install --git"))
        .map(|line| line.trim().to_string())
        .collect()
}

/// Positional crate names on a `cargo install` line, without `@version`.
fn package_names(line: &str) -> Vec<String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut index = 0;
    if tokens.first() == Some(&"cargo") {
        index += 1;
    }
    if tokens.get(index) == Some(&"install") {
        index += 1;
    }
    let mut names = Vec::new();
    while index < tokens.len() {
        let token = tokens[index];
        if token == "--" {
            break;
        }
        if token.starts_with('-') {
            let flag = token.split_once('=').map(|(flag, _)| flag).unwrap_or(token);
            if !token.contains('=') && VALUE_FLAGS.contains(&flag) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        let name = token.split_once('@').map(|(name, _)| name).unwrap_or(token);
        names.push(name.to_string());
        index += 1;
    }
    names
}

fn workspace_binary_packages(root: &Path) -> Vec<String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .current_dir(root)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("spawn cargo metadata");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "cargo metadata --no-deps failed: {stderr}"
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("cargo metadata JSON");
    let members = metadata["workspace_members"]
        .as_array()
        .expect("workspace_members");
    let mut binaries = Vec::new();
    for package in metadata["packages"].as_array().expect("packages") {
        let id = package["id"].as_str().unwrap_or("");
        if !members.iter().any(|member| member.as_str() == Some(id)) {
            continue;
        }
        if !has_bin_target(package) {
            continue;
        }
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        binaries.push(name.to_string());
    }
    binaries.sort();
    binaries
}

fn has_bin_target(package: &Value) -> bool {
    package["targets"].as_array().is_some_and(|targets| {
        targets.iter().any(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")))
        })
    })
}
