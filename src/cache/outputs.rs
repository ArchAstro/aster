//! Pack declared target outputs into the content-addressed store.
//!
//! Declarations are project-relative paths or globs from
//! `[targets.<t>.cache] outputs`. Files become blobs. Directories are walked
//! without following symlinks. A missing declaration or a symlink that leaves
//! the project is a warning and is not stored.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
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

/// Project-relative symlink target, or `None` when the link leaves the project.
///
/// A relative link that stays inside the project is collapsed to that
/// project-relative path, so `../app.js` from `dist/links` becomes
/// `dist/app.js`. An absolute link inside the project is stored the same way.
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
    if !is_safe_declaration(link) {
        return Some(rel);
    }
    normalize_rel(link).ok()
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
        assert_eq!(link.target.as_deref(), Some("dist/app.js"));
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
}
