//! Install-doc checks. Reads repository files only; no network.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_repo_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn installation_section(readme: &str) -> &str {
    let heading = "## Installation";
    let start = readme
        .find(heading)
        .expect("README has an Installation section");
    let rest = &readme[start + heading.len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    &readme[start..start + heading.len() + end]
}

/// Archive names from the release workflow matrix (`artifact: aster-linux-x64`).
fn release_archive_artifacts(workflow: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_matrix = false;
    for line in workflow.lines() {
        let trimmed = line.trim();
        if trimmed == "matrix:" {
            in_matrix = true;
            continue;
        }
        if in_matrix && trimmed == "steps:" {
            break;
        }
        if !in_matrix {
            continue;
        }
        let Some(raw) = trimmed.strip_prefix("artifact:") else {
            continue;
        };
        let name = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        if name.is_empty() || name.contains("${{") {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

fn platform_string(artifact: &str) -> &'static str {
    if artifact.contains("linux-arm64") {
        "linux-arm64"
    } else if artifact.contains("linux-x64") {
        "linux-x64"
    } else if artifact.contains("darwin") || artifact.contains("macos") {
        "macos"
    } else {
        panic!("release artifact {artifact} has no documented platform string");
    }
}

#[test]
fn readme_lists_every_release_archive_platform() {
    let workflows = repo_root().join(".github").join("workflows");
    assert!(
        workflows.is_dir(),
        "missing release workflows directory {}",
        workflows.display()
    );
    let release = workflows.join("release.yml");
    assert!(
        release.is_file(),
        "missing release workflow {}",
        release.display()
    );

    let artifacts = release_archive_artifacts(&read_repo_file(&release));
    assert!(
        !artifacts.is_empty(),
        "release workflow matrix has no archive artifact names"
    );

    let readme = read_repo_file(&repo_root().join("README.md"));
    let installation = installation_section(&readme).to_ascii_lowercase();

    let mut platforms = Vec::new();
    for artifact in &artifacts {
        assert!(
            installation.contains(&artifact.to_ascii_lowercase()),
            "README Installation does not name release asset {artifact}"
        );
        let platform = platform_string(artifact);
        if !platforms.contains(&platform) {
            assert!(
                installation.contains(platform),
                "README Installation does not name platform {platform} (from {artifact})"
            );
            platforms.push(platform);
        }
    }

    for required in ["linux-x64", "linux-arm64", "macos"] {
        assert!(
            platforms.contains(&required),
            "release workflow did not yield platform {required}; artifacts: {artifacts:?}"
        );
    }
    assert!(
        installation.contains("mise use github:archastro/aster@latest"),
        "README Installation should show the mise install command"
    );
    assert!(
        installation.contains("sha256sums"),
        "README Installation should name SHA256SUMS"
    );
    assert!(
        installation.contains("tarball rather than apt"),
        "README Installation should tell arm64 users to use the tarball rather than apt/dnf"
    );
}

#[test]
fn package_repositories_does_not_present_aur_as_live() {
    let doc = read_repo_file(&repo_root().join("docs").join("package-repositories.md"));
    let lower = doc.to_ascii_lowercase();
    assert!(
        lower.contains("not published until") && lower.contains("aur_publish_enabled"),
        "AUR must be described as not published until AUR_PUBLISH_ENABLED is set"
    );
    assert!(
        lower.contains("pacman -u"),
        "pacman -U must remain the supported Arch install path"
    );
    assert!(
        !lower.contains("the `aster-bin` package in the arch user repository"),
        "AUR must not be listed as a live distribution channel"
    );
}

#[test]
fn getting_started_mentions_linux_install() {
    let doc = read_repo_file(&repo_root().join("GETTING_STARTED.MD"));
    let lower = doc.to_ascii_lowercase();
    let step = lower
        .split("## 2.")
        .next()
        .expect("GETTING_STARTED has step 1 before step 2");
    assert!(
        step.contains("linux") && step.contains("aster-linux-arm64"),
        "GETTING_STARTED step 1 should mention a Linux install alternative"
    );
    assert!(
        step.contains("mise use github:archastro/aster@latest"),
        "GETTING_STARTED step 1 should mention mise"
    );
}

/// Documented `cargo install --git` commands must name a workspace binary:
/// without one, cargo refuses a workspace with both `aster` and `aster-srv`.
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
    let root = repo_root();
    let binaries = workspace_binary_packages(&root);

    for relative in DOCS {
        let path = root.join(relative);
        let text = read_repo_file(&path);
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
