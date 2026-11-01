//! Install-doc checks. Reads repository files only; no network.
//! CI docs must install Aster from this repository, not the unrelated crates.io crate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

/// Walk source markdown. Skip `.git` and `target` so the check stays on repo
/// docs and does not follow build output.
fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_markdown(&path, out)?;
        } else if is_markdown(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// `cargo install aster` without `--git` or `--path` resolves the unrelated
/// crates.io package. Flags may appear anywhere on the same line.
fn installs_crates_io_aster(line: &str) -> bool {
    line.contains("cargo install aster") && !line.contains("--git") && !line.contains("--path")
}

#[test]
fn ci_docs_never_install_the_crates_io_aster() {
    let root = repo_root();
    let mut markdown = Vec::new();
    collect_markdown(&root, &mut markdown).expect("walk markdown from the repo root");
    assert!(
        !markdown.is_empty(),
        "expected markdown under {}",
        root.display()
    );

    let mut offenders = Vec::new();
    for path in &markdown {
        let text =
            fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for (index, line) in text.lines().enumerate() {
            if installs_crates_io_aster(line) {
                offenders.push(format!("{}:{}: {line}", path.display(), index + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "cargo install aster without --git or --path:\n{}",
        offenders.join("\n")
    );

    let ci_docs = fs::read_to_string(root.join("docs/ci/github-actions.md"))
        .expect("read docs/ci/github-actions.md");
    assert!(
        !ci_docs.contains("0.21.0"),
        "stale 0.21.0 pin remains in docs/ci/github-actions.md"
    );
    assert!(
        ci_docs.contains(
            "cargo install --git https://github.com/ArchAstro/aster.git --locked --tag v0.30.1 aster"
        ),
        "docs/ci/github-actions.md no longer shows the pinned git install"
    );
}
