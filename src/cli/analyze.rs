//! `aster analyze`: source facts for other tools.

use super::commands::AnalyzeCommands;
use super::output::output_json;
use crate::related::{analyze_change, ChangeScope};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

pub fn run(command: &AnalyzeCommands, cwd: &Path) -> Result<()> {
    match command {
        AnalyzeCommands::Change {
            base,
            head,
            base_tree,
            head_tree,
            paths,
        } => {
            let scope = ChangeScope {
                paths: paths.clone(),
            };
            let report = match (base, head, base_tree, head_tree) {
                (Some(base), Some(head), None, None) => {
                    let base_dir = export(cwd, base)?;
                    let head_dir = export(cwd, head)?;
                    analyze_change(base_dir.path(), head_dir.path(), &scope)
                }
                (None, None, Some(base), Some(head)) => {
                    for dir in [base, head] {
                        if !dir.is_dir() {
                            bail!("{} is not a directory", dir.display());
                        }
                    }
                    analyze_change(base, head, &scope)
                }
                _ => bail!("give --base and --head, or --base-tree and --head-tree"),
            };
            output_json(&report)
        }
    }
}

/// Git run with nothing from the analysed repository's configuration that
/// could start a program: no fsmonitor, no hooks path, no pager.
fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["--no-pager"])
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// The files of `revision` of the repository at `cwd`, written to a
/// temporary directory exactly as stored: blob bytes, read with `ls-tree`
/// and `cat-file --batch`, which apply no filter, `ident`, line-ending or
/// `export-ignore` attribute. Symbolic links and submodules are left out,
/// as a directory walk leaves out links and cannot tell a submodule.
fn export(cwd: &Path, revision: &str) -> Result<tempfile::TempDir> {
    let tree = git(cwd)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{revision}^{{tree}}"))
        .output()
        .context("failed to run git rev-parse")?;
    if !tree.status.success() {
        bail!(
            "{revision} names no tree in the repository at {}",
            cwd.display()
        );
    }
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let listing = git(cwd)
        .args([
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            "--end-of-options",
            &tree,
        ])
        .output()
        .context("failed to run git ls-tree")?;
    if !listing.status.success() {
        bail!(
            "git ls-tree {revision}: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }
    // Each entry is `<mode> <type> <object>\t<path>\0`.
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in listing.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(entry).context("a path in the tree is not UTF-8")?;
        let (meta, path) = text
            .split_once('\t')
            .with_context(|| format!("unexpected ls-tree entry {text:?}"))?;
        let mut fields = meta.split(' ');
        let (Some(mode), Some(kind), Some(object)) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("unexpected ls-tree entry {text:?}");
        };
        if kind != "blob" || mode == "120000" {
            continue;
        }
        let relative = std::path::PathBuf::from(path);
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(name) if name != ".git"))
        {
            bail!("refusing the tree path {path:?}");
        }
        files.push((object.to_string(), relative));
    }
    let dir = tempfile::Builder::new()
        .prefix("aster-change-")
        .tempdir()
        .context("failed to create a temporary directory")?;
    let mut batch = git(cwd)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to run git cat-file")?;
    let mut input = batch.stdin.take().context("no stdin for git cat-file")?;
    let objects: Vec<String> = files.iter().map(|(object, _)| object.clone()).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for object in objects {
            writeln!(input, "{object}")?;
        }
        Ok(())
    });
    let mut output = BufReader::new(batch.stdout.take().context("no stdout for git cat-file")?);
    for (object, relative) in &files {
        // `<object> <type> <size>\n<bytes>\n`
        let mut header = String::new();
        output.read_line(&mut header)?;
        let size: usize = header
            .trim_end()
            .strip_prefix(object.as_str())
            .and_then(|rest| rest.strip_prefix(" blob "))
            .and_then(|size| size.parse().ok())
            .with_context(|| format!("unexpected git cat-file header {header:?}"))?;
        let mut bytes = vec![0; size + 1];
        output.read_exact(&mut bytes)?;
        bytes.pop();
        let target = dir.path().join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, bytes)
            .with_context(|| format!("failed to write {}", target.display()))?;
    }
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("git cat-file writer panicked"))??;
    if !batch.wait()?.success() {
        bail!("git cat-file failed for {revision}");
    }
    Ok(dir)
}
