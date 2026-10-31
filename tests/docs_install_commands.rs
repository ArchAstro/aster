//! CI docs must install Aster from this repository, not the unrelated crates.io crate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
