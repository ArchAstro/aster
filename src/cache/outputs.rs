//! Pack declared target outputs into the content-addressed store, and restore
//! them onto disk from that store.
//!
//! Declarations are project-relative paths or globs from
//! `[targets.<t>.cache] outputs`. Files become blobs. Directories are walked
//! without following symlinks. A missing declaration or a symlink that leaves
//! the project is a warning and is not stored.
//!
//! [`restore`] materializes a packed entry list. It validates every path
//! before deleting anything, replaces each declared output root, and removes
//! those roots again if a blob is missing or corrupt.

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use walkdir::WalkDir;

use super::artifact::{Digest, OutputEntry, OutputKind};
use super::hasher::normalize_rel;
use super::local::LocalStore;

/// Expand `output_globs` and write file blobs into `store`.
///
/// `Ok(None)` means the result must not be stored: a declared output is
/// missing, or a symlink points outside the project. Both print a warning.
/// `Err` is an I/O failure or an invalid glob.
pub fn pack(
    project_root: &Path,
    output_globs: &[String],
    store: &LocalStore,
) -> Result<Option<Vec<OutputEntry>>> {
    if output_globs.is_empty() {
        return Ok(Some(Vec::new()));
    }

    let mut literals = Vec::new();
    let mut glob_builder = GlobSetBuilder::new();
    let mut has_globs = false;
    for pattern in output_globs {
        if is_literal_path(pattern) {
            literals.push(pattern.clone());
            continue;
        }
        let glob =
            Glob::new(pattern).with_context(|| format!("Invalid cache output glob: {pattern}"))?;
        glob_builder.add(glob);
        has_globs = true;
    }

    let mut paths = Vec::new();
    for pattern in &literals {
        let rel = Path::new(pattern);
        if !is_safe_declaration(rel) {
            warn_skip(&format!(
                "cache output `{pattern}` is not a project-relative path"
            ));
            return Ok(None);
        }
        let abs = project_root.join(rel);
        if !exists_no_follow(&abs) {
            warn_skip(&format!("cache output `{pattern}` does not exist"));
            return Ok(None);
        }
        paths.push(abs);
    }

    if has_globs {
        let globs = glob_builder
            .build()
            .context("Failed to build cache output globset")?;
        let matched = match_glob_roots(project_root, &globs)?;
        if matched.is_empty() {
            warn_skip("cache output globs matched no files");
            return Ok(None);
        }
        paths.extend(matched);
    }

    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for path in paths {
        if !pack_tree(project_root, &path, store, &mut entries, &mut seen)? {
            return Ok(None);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Some(entries))
}

/// Replace declared outputs under `project_root` with the packed tree.
///
/// Every entry is checked before the first delete. A path that is absolute,
/// contains `..`, or otherwise leaves `project_root` is refused and nothing
/// is removed. Each declared path is removed (a directory root takes its
/// children with it), then files, directories, and symlinks are written.
/// File bytes come from `store` and land via a same-directory temp rename.
///
/// A missing or corrupt blob, or any write error after a delete, removes the
/// declared paths again and returns `Err`. The caller treats that as a miss.
pub fn restore(project_root: &Path, entries: &[OutputEntry], store: &LocalStore) -> Result<()> {
    let entries = validate_restore_entries(project_root, entries)?;
    let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    remove_outputs(project_root, &paths)?;
    if let Err(error) = write_outputs(project_root, &entries, store) {
        let _ = remove_outputs(project_root, &paths);
        return Err(error);
    }
    Ok(())
}

/// Checked entries, parents before children, so a directory exists before its files.
fn validate_restore_entries(
    project_root: &Path,
    entries: &[OutputEntry],
) -> Result<Vec<OutputEntry>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut checked = Vec::with_capacity(entries.len());
    let mut seen = BTreeSet::new();
    for entry in entries {
        if !is_restore_path(&entry.path) {
            bail!(
                "cache restore path `{}` escapes the project root",
                entry.path
            );
        }
        if path_inside(project_root, &project_root.join(&entry.path)).is_none() {
            bail!(
                "cache restore path `{}` escapes the project root",
                entry.path
            );
        }
        match entry.kind {
            OutputKind::File if entry.digest.is_none() => {
                bail!("cache restore file `{}` is missing a digest", entry.path);
            }
            OutputKind::Symlink => match entry.target.as_deref() {
                Some(target) if symlink_target_stays_inside(&entry.path, target) => {}
                _ => bail!(
                    "cache restore symlink `{}` has a target that escapes the project root",
                    entry.path
                ),
            },
            _ => {}
        }
        if !seen.insert(entry.path.clone()) {
            bail!("cache restore path `{}` is declared twice", entry.path);
        }
        checked.push(entry.clone());
    }
    for entry in &checked {
        refuse_symlinked_ancestor(project_root, &entry.path, &seen)?;
        if let Some(link) = checked.iter().find(|other| {
            other.kind == OutputKind::Symlink && entry.path.starts_with(&format!("{}/", other.path))
        }) {
            bail!(
                "cache restore path `{}` is under symlink `{}`",
                entry.path,
                link.path
            );
        }
    }
    checked.sort_by(|left, right| {
        left.path
            .matches('/')
            .count()
            .cmp(&right.path.matches('/').count())
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(checked)
}

/// Refuse an existing, undeclared ancestor of `rel` that is a symlink, so
/// removal and writes never pass through a link that may point outside the
/// project. A declared ancestor is unlinked (not followed) before its
/// descendants are touched, so checking stops there.
fn refuse_symlinked_ancestor(
    project_root: &Path,
    rel: &str,
    declared: &BTreeSet<String>,
) -> Result<()> {
    let parts: Vec<&str> = rel.split('/').collect();
    for depth in 1..parts.len() {
        let ancestor = parts[..depth].join("/");
        if declared.contains(&ancestor) {
            return Ok(());
        }
        match fs::symlink_metadata(project_root.join(&ancestor)) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("cache restore path `{rel}` is under symlinked directory `{ancestor}`")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to stat cache output parent {ancestor}"));
            }
        }
    }
    Ok(())
}

/// Shallowest first: a declared parent (dir or symlink) is removed before any
/// descendant, so a descendant is never reached through a symlinked parent.
fn remove_outputs(project_root: &Path, paths: &[String]) -> Result<()> {
    let mut ordered = paths.to_vec();
    ordered.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then_with(|| left.cmp(right))
    });
    for rel in ordered {
        let path = project_root.join(&rel);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to stat cache output {rel}"));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove cache output {rel}"))?;
        } else {
            fs::remove_dir_all(&path)
                .with_context(|| format!("Failed to remove cache output {rel}"))?;
        }
    }
    Ok(())
}

fn write_outputs(project_root: &Path, entries: &[OutputEntry], store: &LocalStore) -> Result<()> {
    for entry in entries {
        let path = project_root.join(&entry.path);
        match entry.kind {
            OutputKind::Dir => write_dir(&path, entry.mode)
                .with_context(|| format!("Failed to restore directory {}", entry.path))?,
            OutputKind::File => {
                let digest = entry.digest.as_ref().expect("validated");
                write_file_atomic(&path, digest, entry.mode, store)
                    .with_context(|| format!("Failed to restore file {} from blob", entry.path))?;
            }
            OutputKind::Symlink => {
                let target = entry.target.as_deref().expect("validated");
                write_symlink(&path, target)
                    .with_context(|| format!("Failed to restore symlink {}", entry.path))?;
            }
        }
    }
    Ok(())
}

fn write_dir(path: &Path, mode: u32) -> Result<()> {
    fs::create_dir_all(path)?;
    set_mode(path, mode)
}

/// Write blob bytes to a sibling temp file, then rename over `path`.
fn write_file_atomic(path: &Path, digest: &Digest, mode: u32, store: &LocalStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = restore_temp_path(path);
    let write_result = (|| {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        store.get_blob(digest, &mut file)?;
        file.sync_all()?;
        set_mode(&tmp, mode)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn write_symlink(path: &Path, target: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path)?;
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, path)?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!("symlinks are not supported on this platform");
    }
    Ok(())
}

fn restore_temp_path(final_path: &Path) -> PathBuf {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    final_path.with_file_name(format!(
        ".{name}.aster-restore.{}.{seq}",
        std::process::id()
    ))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::symlink_metadata(path)?.permissions();
        perms.set_mode(mode & 0o777);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Same shape [`super::artifact::Manifest::validate`] accepts, checked here so
/// a hand-built entry list cannot delete outside the project.
fn is_restore_path(path: &str) -> bool {
    if path.is_empty() || path.contains('\\') || path.contains(':') || path.starts_with('/') {
        return false;
    }
    let mut components = Path::new(path).components();
    match components.next() {
        Some(Component::Normal(_)) => {
            components.all(|component| matches!(component, Component::Normal(_)))
        }
        _ => false,
    }
}

/// A packed symlink target is either a project-relative path or the original
/// relative link text. Both must stay inside the project when resolved from
/// the link's parent. Absolute targets and `..` that climb out are refused.
fn symlink_target_stays_inside(link_rel: &str, target: &str) -> bool {
    if !is_restore_path(link_rel) {
        return false;
    }
    if target.is_empty() || target.contains('\\') || target.contains(':') || target.starts_with('/')
    {
        return false;
    }
    let link = Path::new(link_rel);
    let parent = link.parent().unwrap_or(Path::new(""));
    let mut parts: Vec<String> = parent
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Pack `path` and, when it is a directory, its children. Symlinks are not followed.
///
/// `Ok(false)` means a symlink escaped the project and the caller must not store.
fn pack_tree(
    project_root: &Path,
    path: &Path,
    store: &LocalStore,
    entries: &mut Vec<OutputEntry>,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<bool> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("Failed to stat {}", path.display()))?;
    let rel = project_relative(project_root, path)?;
    if !seen.insert(rel.clone()) {
        return Ok(true);
    }

    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return pack_symlink(project_root, path, &rel, mode_of(&metadata), entries);
    }
    if file_type.is_dir() {
        entries.push(OutputEntry {
            path: rel,
            kind: OutputKind::Dir,
            digest: None,
            mode: mode_of(&metadata),
            target: None,
        });
        let mut children: Vec<PathBuf> = fs::read_dir(path)
            .with_context(|| format!("Failed to read {}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("Failed to read {}", path.display()))?;
        children.sort();
        for child in children {
            if !pack_tree(project_root, &child, store, entries, seen)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if file_type.is_file() {
        entries.push(pack_file(path, &rel, mode_of(&metadata), store)?);
        return Ok(true);
    }
    warn_skip(&format!(
        "cache output `{rel}` is not a file, directory, or symlink"
    ));
    Ok(false)
}

fn pack_file(path: &Path, rel: &str, mode: u32, store: &LocalStore) -> Result<OutputEntry> {
    let mut file =
        File::open(path).with_context(|| format!("Failed to open cache output {rel}"))?;
    let digest = Digest::of_reader(&mut file)
        .with_context(|| format!("Failed to hash cache output {rel}"))?;
    file.seek_read_from_start()
        .with_context(|| format!("Failed to rewind cache output {rel}"))?;
    store
        .put_blob(&digest, file)
        .with_context(|| format!("Failed to store cache output {rel}"))?;
    Ok(OutputEntry {
        path: rel.to_string(),
        kind: OutputKind::File,
        digest: Some(digest),
        mode,
        target: None,
    })
}

/// Store a symlink without following it. An external target rejects the pack.
fn pack_symlink(
    project_root: &Path,
    path: &Path,
    rel: &str,
    mode: u32,
    entries: &mut Vec<OutputEntry>,
) -> Result<bool> {
    let link = fs::read_link(path)
        .with_context(|| format!("Failed to read symlink {}", path.display()))?;
    let Some(target) = symlink_target(project_root, path, &link) else {
        warn_skip(&format!(
            "cache output `{rel}` is a symlink to {} outside the project",
            link.display()
        ));
        return Ok(false);
    };
    entries.push(OutputEntry {
        path: rel.to_string(),
        kind: OutputKind::Symlink,
        digest: None,
        mode,
        target: Some(target),
    });
    Ok(true)
}

/// Project-relative check, then the text restore should write.
///
/// A relative link that stays inside the project is stored as that link text,
/// so `../app.js` from `dist/links` stays `../app.js`. An absolute link inside
/// the project is stored as the project-relative path it names.
fn symlink_target(project_root: &Path, link_path: &Path, link: &Path) -> Option<String> {
    let resolved = if link.is_absolute() {
        link.to_path_buf()
    } else {
        link_path.parent().unwrap_or(project_root).join(link)
    };
    let rel = path_inside(project_root, &resolved)?;
    if link.is_absolute() {
        return Some(rel);
    }
    let text = link.to_str()?;
    if text.is_empty() || text.contains('\\') || text.contains(':') {
        return None;
    }
    Some(text.replace('\\', "/"))
}

/// Walk the project once and return glob matches that are not under another match.
///
/// A directory declaration is expanded by [`pack_tree`]. Matching its children
/// as well would pack the same tree twice.
fn match_glob_roots(project_root: &Path, globs: &GlobSet) -> Result<Vec<PathBuf>> {
    let mut matched = Vec::new();
    for entry in WalkDir::new(project_root).follow_links(false) {
        let entry = entry.with_context(|| {
            format!(
                "Failed to walk {} for cache outputs",
                project_root.display()
            )
        })?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = match entry.path().strip_prefix(project_root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if !globs.is_match(rel) {
            continue;
        }
        let covered = matched
            .iter()
            .any(|root: &PathBuf| entry.path().starts_with(root) && entry.path() != root.as_path());
        if !covered {
            matched.push(entry.path().to_path_buf());
        }
    }
    Ok(matched)
}

fn project_relative(project_root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(project_root).with_context(|| {
        format!(
            "cache output {} is outside {}",
            path.display(),
            project_root.display()
        )
    })?;
    normalize_rel(rel)
}

/// Lexically resolve `path` against `root`. `None` if it escapes `root`.
///
/// `root` may itself contain `..` (a temp dir under a scratch symlink does).
/// Those components are kept as the root prefix; only `path` is collapsed,
/// and it may not climb out of that prefix.
fn path_inside(root: &Path, path: &Path) -> Option<String> {
    let mut root_parts = Vec::new();
    for component in root.components() {
        match component {
            Component::Normal(part) => root_parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                root_parts.push("..".to_string());
            }
            Component::RootDir | Component::Prefix(_) => root_parts.clear(),
        }
    }

    let mut parts = root_parts.clone();
    for component in path.strip_prefix(root).unwrap_or(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.len() <= root_parts.len() {
                    return None;
                }
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts[root_parts.len()..].join("/"))
}

fn is_literal_path(pattern: &str) -> bool {
    !pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | '{'))
}

fn is_safe_declaration(path: &Path) -> bool {
    let text = path.to_str().unwrap_or("");
    if text.is_empty() || text.contains('\\') || text.contains(':') || path.is_absolute() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn mode_of(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if metadata.is_dir() {
            0o755
        } else {
            0o644
        }
    }
}

fn warn_skip(message: &str) {
    eprintln!("Warning: {message}; result not stored");
}

/// Rewind a file after hashing it, without pulling `Seek` into the signature.
trait Rewind {
    fn seek_read_from_start(&mut self) -> std::io::Result<()>;
}

impl Rewind for File {
    fn seek_read_from_start(&mut self) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        self.seek(SeekFrom::Start(0)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn pack_file_nested_dir_and_executable_bit() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/nested/app.js"), b"app");
        let script = project.path().join("dist/run.sh");
        write_file(&script, b"#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");

        let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            ["dist", "dist/nested", "dist/nested/app.js", "dist/run.sh"]
        );
        assert_eq!(entries[0].kind, OutputKind::Dir);
        assert!(entries[0].digest.is_none());
        let app = entries
            .iter()
            .find(|entry| entry.path == "dist/nested/app.js")
            .unwrap();
        assert_eq!(app.kind, OutputKind::File);
        assert_eq!(app.digest, Some(Digest::of_bytes(b"app")));
        assert!(store.has_blob(app.digest.as_ref().unwrap()));
        let mut restored = Vec::new();
        store
            .get_blob(app.digest.as_ref().unwrap(), &mut restored)
            .unwrap();
        assert_eq!(restored, b"app");

        let run = entries
            .iter()
            .find(|entry| entry.path == "dist/run.sh")
            .unwrap();
        #[cfg(unix)]
        assert_eq!(run.mode & 0o111, 0o111);
        #[cfg(not(unix))]
        assert_eq!(run.mode, 0o644);
    }

    #[test]
    fn pack_stores_internal_symlink_and_file_blob() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/app.js"), b"app");
        #[cfg(unix)]
        std::os::unix::fs::symlink("app.js", project.path().join("dist/current")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("app.js", project.path().join("dist/current")).unwrap();

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let link = entries
            .iter()
            .find(|entry| entry.path == "dist/current")
            .expect("symlink entry");
        assert_eq!(link.kind, OutputKind::Symlink);
        assert_eq!(link.target.as_deref(), Some("app.js"));
        assert!(link.digest.is_none());
        assert!(store.has_blob(&Digest::of_bytes(b"app")));
    }

    #[test]
    fn pack_expands_a_file_glob_without_packing_the_directory() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/nested/app.js"), b"app");
        write_file(&project.path().join("dist/nested/app.css"), b"css");

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist/**/*.js".to_string()], &store)
            .unwrap()
            .expect("stored");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "dist/nested/app.js");
        assert_eq!(entries[0].digest, Some(Digest::of_bytes(b"app")));
        assert!(!store.has_blob(&Digest::of_bytes(b"css")));
    }

    #[test]
    fn pack_keeps_parent_relative_symlink_inside_the_project() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/app.js"), b"app");
        fs::create_dir_all(project.path().join("dist/links")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../app.js", project.path().join("dist/links/current")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("../app.js", project.path().join("dist/links/current"))
            .unwrap();

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let link = entries
            .iter()
            .find(|entry| entry.path == "dist/links/current")
            .unwrap();
        assert_eq!(link.target.as_deref(), Some("../app.js"));
    }

    #[test]
    fn pack_rejects_missing_output_and_external_symlink() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let store = LocalStore::new(cache.path().to_path_buf());

        let missing = pack(project.path(), &["dist/missing.txt".to_string()], &store).unwrap();
        assert!(missing.is_none());

        write_file(&project.path().join("inside.txt"), b"in");
        let outside = tempfile::tempdir().unwrap();
        write_file(&outside.path().join("secret"), b"nope");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret"), project.path().join("leak"))
            .unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(
            outside.path().join("secret"),
            project.path().join("leak"),
        )
        .unwrap();

        let rejected = pack(project.path(), &["leak".to_string()], &store).unwrap();
        assert!(rejected.is_none());
        assert!(!store.has_blob(&Digest::of_bytes(b"nope")));
    }

    fn snapshot(project: &Path, root_rel: &str) -> Vec<(String, String, u32)> {
        let root = project.join(root_rel);
        let mut rows = Vec::new();
        for entry in WalkDir::new(&root).follow_links(false).sort_by_file_name() {
            let entry = entry.unwrap();
            if entry.depth() == 0 {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            let kind = if meta.file_type().is_symlink() {
                let link = fs::read_link(entry.path()).unwrap();
                let resolved = if link.is_absolute() {
                    link.clone()
                } else {
                    entry.path().parent().unwrap_or(project).join(&link)
                };
                let inside =
                    path_inside(project, &resolved).unwrap_or_else(|| link.display().to_string());
                format!("link:{inside}")
            } else if meta.file_type().is_dir() {
                "dir".to_string()
            } else {
                format!("file:{}", fs::read_to_string(entry.path()).unwrap())
            };
            rows.push((rel, kind, mode_of(&meta)));
        }
        rows
    }

    #[test]
    fn restore_round_trip_matches_packed_tree_including_modes() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/nested/app.js"), b"app");
        let script = project.path().join("dist/run.sh");
        write_file(&script, b"#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
            std::os::unix::fs::symlink("../app.js", project.path().join("dist/nested/current"))
                .unwrap();
        }
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("../app.js", project.path().join("dist/nested/current"))
            .unwrap();

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let packed = snapshot(project.path(), "dist");

        fs::remove_dir_all(project.path().join("dist")).unwrap();
        write_file(&project.path().join("dist/stale.txt"), b"stale");
        write_file(&project.path().join("keep.txt"), b"keep");

        restore(project.path(), &entries, &store).unwrap();

        assert_eq!(snapshot(project.path(), "dist"), packed);
        assert_eq!(fs::read(project.path().join("keep.txt")).unwrap(), b"keep");
        assert!(!project.path().join("dist/stale.txt").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(project.path().join("dist/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
            assert_eq!(
                fs::read_link(project.path().join("dist/nested/current")).unwrap(),
                Path::new("../app.js")
            );
        }
    }

    #[test]
    fn restore_aborts_on_corrupt_blob_and_leaves_no_partial_output() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/a.txt"), b"aaa");
        write_file(&project.path().join("dist/b.txt"), b"bbb");
        write_file(&project.path().join("keep.txt"), b"keep");

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        fs::remove_dir_all(project.path().join("dist")).unwrap();

        let b = entries
            .iter()
            .find(|entry| entry.path == "dist/b.txt")
            .unwrap();
        let digest = b.digest.as_ref().unwrap();
        let blob = store.blob_path(digest).unwrap();
        fs::write(&blob, b"not-a-zstd-frame").unwrap();

        let error = restore(project.path(), &entries, &store).unwrap_err();
        assert!(error.to_string().contains("dist"), "{error}");
        assert!(
            !project.path().join("dist").exists(),
            "partial restore left dist behind"
        );
        assert_eq!(fs::read(project.path().join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn restore_refuses_path_that_escapes_project_root() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("keep.txt"), b"keep");
        let store = LocalStore::new(cache.path().to_path_buf());

        for path in ["../outside.txt", "/etc/passwd", "dist/../../etc/passwd", ""] {
            let entries = vec![OutputEntry {
                path: path.to_string(),
                kind: OutputKind::File,
                digest: Some(Digest::of_bytes(b"x")),
                mode: 0o644,
                target: None,
            }];
            let error = restore(project.path(), &entries, &store).unwrap_err();
            assert!(
                error.to_string().contains("escapes"),
                "path {path:?} produced {error}"
            );
        }
        assert_eq!(fs::read(project.path().join("keep.txt")).unwrap(), b"keep");

        let link = vec![OutputEntry {
            path: "dist/leak".into(),
            kind: OutputKind::Symlink,
            digest: None,
            mode: 0o777,
            target: Some("../../outside".into()),
        }];
        assert!(restore(project.path(), &link, &store).is_err());
        assert!(!project.path().join("dist").exists());
    }

    #[test]
    fn restore_same_directory_symlink_stays_inside_the_project() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/app.js"), b"app");
        #[cfg(unix)]
        std::os::unix::fs::symlink("app.js", project.path().join("dist/current")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("app.js", project.path().join("dist/current")).unwrap();

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        restore(project.path(), &entries, &store).unwrap();

        let link = fs::read_link(project.path().join("dist/current")).unwrap();
        assert_eq!(link, Path::new("app.js"));
        let resolved = project.path().join("dist").join(link);
        assert!(path_inside(project.path(), &resolved).is_some());
    }

    #[test]
    fn restore_removes_a_directory_symlink_without_following_it() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_file(&outside.path().join("secret"), b"keep-me");
        write_file(&project.path().join("dist/app.js"), b"app");

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist/app.js".to_string()], &store)
            .unwrap()
            .expect("stored");
        fs::remove_file(project.path().join("dist/app.js")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), project.path().join("dist/app.js")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), project.path().join("dist/app.js"))
            .unwrap();

        restore(project.path(), &entries, &store).unwrap();
        assert_eq!(
            fs::read(project.path().join("dist/app.js")).unwrap(),
            b"app"
        );
        assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"keep-me");
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_output_under_a_symlinked_parent_directory() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/app.js"), b"app");

        let store = LocalStore::new(cache.path().to_path_buf());
        let entries = pack(project.path(), &["dist/app.js".to_string()], &store)
            .unwrap()
            .expect("stored");
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        write_file(&outside.path().join("app.js"), b"outside");
        std::os::unix::fs::symlink(outside.path(), project.path().join("dist")).unwrap();

        let error = restore(project.path(), &entries, &store).unwrap_err();
        assert!(error.to_string().contains("symlinked directory"), "{error}");
        assert_eq!(fs::read(outside.path().join("app.js")).unwrap(), b"outside");
        assert!(fs::symlink_metadata(project.path().join("dist"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
