//! Pack declared target outputs into the content-addressed store, and restore
//! them onto disk from that store.
//!
//! Declarations are project-relative paths or globs from
//! `[targets.<t>.cache] outputs`. Directories are walked without following
//! symlinks. A missing declaration or a symlink that leaves the project is a
//! warning and is not stored.
//!
//! The regular files of one target become one archive: a tar stream, zstd
//! compressed and cut into chunks that are stored as blobs (see
//! [`super::chunks`]). A member is named by the index of its entry in the
//! output tree, so a restore decides where each file goes and an archive
//! cannot name a path. Directories and symlinks exist only in the tree.
//!
//! [`restore`] materializes a packed tree in place. It validates every path
//! and checks every chunk is stored before changing anything, rewrites only
//! outputs that differ, prunes what a declared directory no longer holds, and
//! removes the declared paths if the archive turns out corrupt.

use crate::errln;
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest as ShaDigest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use walkdir::WalkDir;

use super::artifact::{chunk_path, Digest, OutputArchive, OutputEntry, OutputKind};
use super::chunks::write_chunked;
use super::hasher::{encode_hex, normalize_rel};
use super::local::LocalStore;

/// The output tree of one target and the chunks that hold its file bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// Every declared file, directory and symlink, sorted by path. Each file
    /// carries the digest of its bytes.
    pub tree: Vec<OutputEntry>,
    /// Archive chunk digests in stream order. Empty when the tree holds no
    /// regular file, so there is nothing to archive.
    pub chunks: Vec<Digest>,
}

impl Packed {
    /// The manifest's `outputs` and `archive`.
    ///
    /// An archived tree names only its chunks in `outputs`; a tree of
    /// directories and symlinks has no bytes to store and stays in `outputs`.
    pub fn into_manifest_parts(self) -> (Vec<OutputEntry>, Option<OutputArchive>) {
        if self.chunks.is_empty() {
            return (self.tree, None);
        }
        let outputs = self
            .chunks
            .into_iter()
            .enumerate()
            .map(|(index, digest)| OutputEntry {
                path: chunk_path(index),
                kind: OutputKind::ArchiveChunk,
                digest: Some(digest),
                mode: 0o644,
                target: None,
            })
            .collect();
        (outputs, Some(OutputArchive { entries: self.tree }))
    }
}

/// Expand `output_globs`, archive the files they select and write the
/// archive's chunks into `store`.
///
/// `Ok(None)` means the result must not be stored: a declared output is
/// missing, or a symlink points outside the project. Both print a warning.
/// `Err` is an I/O failure or an invalid glob.
pub fn pack(
    project_root: &Path,
    output_globs: &[String],
    store: &LocalStore,
) -> Result<Option<Packed>> {
    if output_globs.is_empty() {
        return Ok(Some(Packed {
            tree: Vec::new(),
            chunks: Vec::new(),
        }));
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
        if !pack_tree(project_root, &path, &mut entries, &mut seen)? {
            return Ok(None);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let chunks = archive_files(project_root, &mut entries, store)?;
    Ok(Some(Packed {
        tree: entries,
        chunks,
    }))
}

/// Stream every file entry into one tar, zstd and chunk it, and fill in each
/// entry's digest from the bytes that went into the archive. Returns the
/// chunk digests, none when there is no file.
///
/// The archive is deterministic (no times, owners or names beyond the member
/// index), so unchanged outputs produce the same chunk blobs.
fn archive_files(
    project_root: &Path,
    entries: &mut [OutputEntry],
    store: &LocalStore,
) -> Result<Vec<Digest>> {
    if !entries.iter().any(|entry| entry.kind == OutputKind::File) {
        return Ok(Vec::new());
    }
    write_chunked(
        |digest, bytes| {
            store
                .put_blob(digest, bytes)
                .with_context(|| format!("Failed to store archive chunk {}", digest.as_str()))
        },
        |sink| {
            let encoder = zstd::stream::write::Encoder::new(sink, ARCHIVE_ZSTD_LEVEL)
                .context("Failed to start the output archive")?;
            let mut tar = tar::Builder::new(encoder);
            for (index, entry) in entries.iter_mut().enumerate() {
                if entry.kind == OutputKind::File {
                    append_file(&mut tar, project_root, index, entry)?;
                }
            }
            let encoder = tar
                .into_inner()
                .context("Failed to finish the output archive")?;
            encoder
                .finish()
                .context("Failed to finish the output archive")?;
            Ok(())
        },
    )
}

/// Same trade as the directory cache: fast enough that packing never shows
/// next to the build, and build outputs compress well at this level.
const ARCHIVE_ZSTD_LEVEL: i32 = 3;

/// Add `entry`'s file as member `index` and record the digest of the bytes
/// that were archived.
fn append_file<W: Write>(
    tar: &mut tar::Builder<W>,
    project_root: &Path,
    index: usize,
    entry: &mut OutputEntry,
) -> Result<()> {
    let path = project_root.join(&entry.path);
    let file =
        File::open(&path).with_context(|| format!("Failed to open cache output {}", entry.path))?;
    let length = file
        .metadata()
        .with_context(|| format!("Failed to stat cache output {}", entry.path))?
        .len();
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(length);
    header.set_mode(entry.mode & 0o777);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    // Exactly `length` bytes, hashed as they go: a file that changes while it
    // is read cannot desynchronize the stream, and the digest is of what was
    // stored.
    let mut reader = HashingReader::new(file.take(length), length);
    tar.append_data(&mut header, index.to_string(), &mut reader)
        .with_context(|| format!("Failed to archive cache output {}", entry.path))?;
    entry.digest = Some(reader.finish());
    Ok(())
}

/// Hashes what passes through, and fails if the source ends before `remaining`.
struct HashingReader<R> {
    inner: R,
    remaining: u64,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R, remaining: u64) -> Self {
        Self {
            inner,
            remaining,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> Digest {
        Digest::from_hex(encode_hex(ShaDigest::finalize(self.hasher).as_ref()))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let read = self.inner.read(buffer)?;
        if read == 0 {
            return Err(io::Error::other("a file shrank while it was being stored"));
        }
        self.remaining -= read as u64;
        ShaDigest::update(&mut self.hasher, &buffer[..read]);
        Ok(read)
    }
}

/// Make declared outputs under `project_root` match the packed tree.
///
/// Every entry is checked before anything on disk changes. A path that is
/// absolute, contains `..`, or otherwise leaves `project_root` is refused and
/// nothing is touched. An archive chunk missing from `store` is refused the
/// same way.
///
/// Restore updates outputs in place, so other processes can read them while it
/// runs. Several restores of the same entry can also run at once. An output
/// that already matches its entry is left alone. A changed file is written to a
/// sibling temp file and renamed over the old one. A path whose type changed
/// (a file that is now a directory) is removed first. Then anything under a
/// declared directory that the entry list does not name is removed, so stale
/// files do not survive a hit. Another restore's in-flight temp files are
/// skipped.
///
/// The files that need writing come out of one pass over the archive. The
/// pass reads every chunk through the end of the last file it needs, even
/// when most files already match, because the stream cannot be entered in the
/// middle. When every file already matches, the archive is not read at all.
/// Each file is hashed as it is written and must equal its entry's digest.
/// Files are not fsynced: a crash can leave a truncated file, and the next
/// restore of this entry finds the digest wrong and rewrites it.
///
/// A corrupt chunk, a file that fails its digest or a write error removes the
/// declared paths and returns `Err`. The caller treats that as a miss and runs
/// the target.
pub fn restore(
    project_root: &Path,
    tree: &[OutputEntry],
    chunks: &[Digest],
    store: &LocalStore,
) -> Result<()> {
    let entries = validate_restore_entries(project_root, tree)?;
    if let Some(chunk) = chunks.iter().find(|chunk| !store.has_blob(chunk)) {
        bail!(
            "cache restore archive chunk {} has no blob in the store",
            chunk.as_str()
        );
    }
    if chunks.is_empty()
        && entries
            .iter()
            .any(|(_, entry)| entry.kind == OutputKind::File)
    {
        bail!("cache restore has files but no archive");
    }
    let result = write_outputs(project_root, &entries, chunks, store)
        .and_then(|()| prune_undeclared(project_root, &entries));
    if let Err(error) = result {
        let paths: Vec<String> = entries
            .iter()
            .map(|(_, entry)| entry.path.clone())
            .collect();
        let _ = remove_outputs(project_root, &paths);
        return Err(error);
    }
    Ok(())
}

/// Checked entries with their index in the packed tree, parents before
/// children, so a directory exists before its files. The index names the
/// entry's archive member.
fn validate_restore_entries(
    project_root: &Path,
    entries: &[OutputEntry],
) -> Result<Vec<(usize, OutputEntry)>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut checked = Vec::with_capacity(entries.len());
    let mut seen = BTreeSet::new();
    for (index, entry) in entries.iter().enumerate() {
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
        if entry.kind == OutputKind::ArchiveChunk {
            bail!("cache restore path `{}` is an archive chunk", entry.path);
        }
        if !seen.insert(entry.path.clone()) {
            bail!("cache restore path `{}` is declared twice", entry.path);
        }
        checked.push((index, entry.clone()));
    }
    for (_, entry) in &checked {
        refuse_symlinked_ancestor(project_root, &entry.path, &seen)?;
        if let Some((_, link)) = checked.iter().find(|(_, other)| {
            other.kind == OutputKind::Symlink && entry.path.starts_with(&format!("{}/", other.path))
        }) {
            bail!(
                "cache restore path `{}` is under symlink `{}`",
                entry.path,
                link.path
            );
        }
    }
    checked.sort_by(|(_, left), (_, right)| {
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

/// Directories and symlinks first, parents before children (see
/// [`validate_restore_entries`]), noting which files differ from the tree.
/// Then one pass over the archive writes those files.
fn write_outputs(
    project_root: &Path,
    entries: &[(usize, OutputEntry)],
    chunks: &[Digest],
    store: &LocalStore,
) -> Result<()> {
    // Archive member index -> the entry whose bytes it holds.
    let mut stale: BTreeMap<usize, &OutputEntry> = BTreeMap::new();
    for (index, entry) in entries {
        let path = project_root.join(&entry.path);
        match entry.kind {
            OutputKind::Dir => write_dir(&path, entry.mode)
                .with_context(|| format!("Failed to restore directory {}", entry.path))?,
            OutputKind::File => {
                let digest = entry.digest.as_ref().expect("validated");
                if file_matches(&path, digest, entry.mode)? {
                    continue;
                }
                remove_if_real_dir(&path)?;
                stale.insert(*index, entry);
            }
            OutputKind::Symlink => {
                let target = entry.target.as_deref().expect("validated");
                if fs::read_link(&path).is_ok_and(|current| current == Path::new(target)) {
                    continue;
                }
                remove_if_real_dir(&path)?;
                write_symlink_atomic(&path, target)
                    .with_context(|| format!("Failed to restore symlink {}", entry.path))?;
            }
            OutputKind::ArchiveChunk => unreachable!("validated"),
        }
    }
    if stale.is_empty() {
        return Ok(());
    }
    let files = entries
        .iter()
        .filter(|(_, entry)| entry.kind == OutputKind::File)
        .count();
    unpack_archive(project_root, &stale, files, chunks, store)
}

/// Read the archive once and write the members named in `stale`. `files` is
/// how many file members the tree has: all of them must be present.
fn unpack_archive(
    project_root: &Path,
    stale: &BTreeMap<usize, &OutputEntry>,
    files: usize,
    chunks: &[Digest],
    store: &LocalStore,
) -> Result<()> {
    let decoder = zstd::stream::read::Decoder::new(ChunkReader::new(store, chunks))
        .context("Failed to open the output archive")?;
    let mut archive = tar::Archive::new(decoder);
    let mut seen = 0_usize;
    let mut written = 0_usize;
    let mut previous: Option<usize> = None;
    for member in archive
        .entries()
        .context("Failed to read the output archive")?
    {
        let mut member = member.context("cache output archive is unreadable or corrupt")?;
        let index = member_index(&member)?;
        if previous.is_some_and(|previous| index <= previous) {
            bail!("output archive members are out of order at {index}");
        }
        previous = Some(index);
        seen += 1;
        let Some(entry) = stale.get(&index) else {
            // Already on disk, or not a file: the next iteration skips its bytes.
            continue;
        };
        let digest = entry.digest.as_ref().expect("validated");
        write_member_atomic(
            &project_root.join(&entry.path),
            &mut member,
            digest,
            entry.mode,
        )
        .with_context(|| format!("Failed to restore file {} from the archive", entry.path))?;
        written += 1;
    }
    if written != stale.len() || seen != files {
        bail!(
            "output archive holds {seen} of {files} files and {written} of {} needed",
            stale.len()
        );
    }
    Ok(())
}

/// The tree index a member is named by: canonical decimal, nothing else.
fn member_index<R: Read>(member: &tar::Entry<'_, R>) -> Result<usize> {
    if member.header().entry_type() != tar::EntryType::Regular {
        bail!("output archive holds a member that is not a regular file");
    }
    let path = member.path().context("Failed to read a member name")?;
    let name = path.to_str().unwrap_or_default();
    let index: usize = name
        .parse()
        .with_context(|| format!("output archive member `{name}` is not an index"))?;
    if index.to_string() != name {
        bail!("output archive member `{name}` is not an index");
    }
    Ok(index)
}

/// The archive's chunks as one byte stream. Each chunk is read from the store
/// whole when the stream reaches it, which checks its digest, and is dropped
/// when the next one is loaded.
struct ChunkReader<'a> {
    store: &'a LocalStore,
    pending: std::slice::Iter<'a, Digest>,
    current: io::Cursor<Vec<u8>>,
}

impl<'a> ChunkReader<'a> {
    fn new(store: &'a LocalStore, chunks: &'a [Digest]) -> Self {
        Self {
            store,
            pending: chunks.iter(),
            current: io::Cursor::new(Vec::new()),
        }
    }
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = self.current.read(buffer)?;
            if read > 0 || buffer.is_empty() {
                return Ok(read);
            }
            let Some(digest) = self.pending.next() else {
                return Ok(0);
            };
            let mut bytes = std::mem::take(self.current.get_mut());
            bytes.clear();
            self.store
                .get_blob(digest, &mut bytes)
                .map_err(io::Error::other)?;
            self.current = io::Cursor::new(bytes);
        }
    }
}

/// Create `path` as a real directory with `mode`. A file or symlink in the
/// way is unlinked, never followed.
fn write_dir(path: &Path, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if mode_matches(&metadata, mode) {
                return Ok(());
            }
        }
        Ok(_) => {
            remove_path(path)?;
            create_dir_tolerating_race(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_dir_tolerating_race(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    set_mode(path, mode)
}

/// Create `path` as a directory, accepting one another restore created first.
///
/// Only a real directory counts: a symlink to a directory that appeared in the
/// race is unlinked rather than followed, so later writes beneath `path` cannot
/// be redirected outside the project.
fn create_dir_tolerating_race(path: &Path) -> Result<()> {
    const ATTEMPTS: usize = 4;
    let mut last_error = None;
    for _ in 0..ATTEMPTS {
        match fs::create_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_real_dir(path)? {
                    // Another restore created it first.
                    return Ok(());
                }
                remove_path(path)?;
                last_error = Some(error);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(path)?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(last_error.expect("at least one attempt").into())
}

/// True when `path` is a directory itself, not a symlink to one.
fn is_real_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// True when `path` is a regular file (not a symlink) with `digest` and `mode`.
fn file_matches(path: &Path, digest: &Digest, mode: u32) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || !mode_matches(&metadata, mode) {
        return Ok(false);
    }
    let file = match File::open(path) {
        Ok(file) => file,
        // Replaced or removed since the stat: rewrite it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(Digest::of_reader(file)? == *digest)
}

/// A rename cannot replace a directory, so a real directory where a file or
/// symlink belongs is removed first. Symlinks are left for the rename, which
/// replaces the link itself.
fn remove_if_real_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            remove_path(path)
        }
        _ => Ok(()),
    }
}

/// Remove whatever is at `path` without following a symlink.
///
/// Another restore may change the same path between the stat and the remove
/// (a directory it just replaced with a file, say). The type is read again and
/// the remove retried, and a path that is already gone counts as removed, so
/// that race does not fail this restore and trigger its cleanup.
fn remove_path(path: &Path) -> Result<()> {
    const ATTEMPTS: usize = 4;
    let mut last_error = None;
    for _ in 0..ATTEMPTS {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("at least one attempt").into())
}

/// Remove what a declared directory holds that the entry list does not name.
///
/// Only children of `Dir` entries are inspected. A glob of files declares no
/// directory, so nothing beside those files is touched. An undeclared child
/// directory goes with its contents. Symlinks are unlinked, not followed.
fn prune_undeclared(project_root: &Path, entries: &[(usize, OutputEntry)]) -> Result<()> {
    let declared: BTreeSet<&str> = entries
        .iter()
        .map(|(_, entry)| entry.path.as_str())
        .collect();
    for (_, entry) in entries
        .iter()
        .filter(|(_, entry)| entry.kind == OutputKind::Dir)
    {
        let dir = project_root.join(&entry.path);
        let children = match fs::read_dir(&dir) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read cache output {}", entry.path));
            }
        };
        for child in children {
            let child = child.with_context(|| format!("Failed to read {}", entry.path))?;
            let name = child.file_name();
            // Manifest paths are UTF-8, so a non-UTF-8 name is never declared.
            let rel = name.to_str().map(|name| format!("{}/{name}", entry.path));
            let keep = rel.as_deref().is_some_and(|rel| {
                declared.contains(rel) || is_live_restore_temp(&rel[entry.path.len() + 1..], &child)
            });
            if keep {
                continue;
            }
            let rel = rel.unwrap_or_else(|| child.path().display().to_string());
            remove_path(&child.path())
                .with_context(|| format!("Failed to remove stale cache output {rel}"))?;
        }
    }
    Ok(())
}

/// How long a restore temp file is assumed to belong to a running restore.
/// Older ones are leftovers of a killed process and are pruned.
const RESTORE_TEMP_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

fn is_live_restore_temp(name: &str, entry: &fs::DirEntry) -> bool {
    if !(name.starts_with('.') && name.contains(RESTORE_TEMP_MARKER)) {
        return false;
    }
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_none_or(|age| age < RESTORE_TEMP_GRACE)
}

/// Copy `source` to a sibling temp file, check its digest, then rename it
/// over `path`. No fsync: see [`restore`].
fn write_member_atomic(
    path: &Path,
    source: &mut impl Read,
    digest: &Digest,
    mode: u32,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = restore_temp_path(path);
    let write_result = (|| {
        let file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        let mut writer = HashingWriter {
            inner: BufWriter::with_capacity(128 * 1024, file),
            hasher: Sha256::new(),
        };
        io::copy(source, &mut writer)?;
        writer.inner.flush()?;
        let hashed = encode_hex(ShaDigest::finalize(writer.hasher).as_ref());
        if hashed != digest.as_str() {
            bail!(
                "restored bytes do not match digest {} (got {hashed})",
                digest.as_str()
            );
        }
        set_mode(&tmp, mode)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        ShaDigest::update(&mut self.hasher, &bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Create the link under a sibling temp name, then rename it over `path`.
fn write_symlink_atomic(path: &Path, target: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = restore_temp_path(path);
    let write_result = (|| {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, &tmp)?;
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, &tmp)?;
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = target;
            bail!("symlinks are not supported on this platform");
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// Every restore temp name contains this; see [`restore_temp_path`].
const RESTORE_TEMP_MARKER: &str = ".aster-restore.";

fn restore_temp_path(final_path: &Path) -> PathBuf {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    final_path.with_file_name(format!(
        ".{name}{RESTORE_TEMP_MARKER}{}.{seq}",
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
            if !pack_tree(project_root, &child, entries, seen)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if file_type.is_file() {
        entries.push(OutputEntry {
            path: rel,
            kind: OutputKind::File,
            // Filled in as the file goes into the archive.
            digest: None,
            mode: mode_of(&metadata),
            target: None,
        });
        return Ok(true);
    }
    warn_skip(&format!(
        "cache output `{rel}` is not a file, directory, or symlink"
    ));
    Ok(false)
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

/// Unix permission bits match. Other platforms do not store them, so a
/// unix-packed 0o755 would otherwise never match and be rewritten every hit.
fn mode_matches(metadata: &fs::Metadata, mode: u32) -> bool {
    !cfg!(unix) || mode_of(metadata) == mode & 0o777
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
    errln!("Warning: {message}; result not stored");
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

    #[cfg(unix)]
    #[test]
    fn create_dir_race_replaces_symlink_to_directory() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = project.path().join("dist");
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();

        create_dir_tolerating_race(&path).unwrap();

        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert!(outside.path().exists());
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
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();

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
        // Two files, one archive: a single chunk, and no blob of its own per file.
        assert_eq!(packed.chunks.len(), 1);
        assert!(store.has_blob(&packed.chunks[0]));
        assert!(!store.has_blob(app.digest.as_ref().unwrap()));

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
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        let link = entries
            .iter()
            .find(|entry| entry.path == "dist/current")
            .expect("symlink entry");
        assert_eq!(link.kind, OutputKind::Symlink);
        assert_eq!(link.target.as_deref(), Some("app.js"));
        assert!(link.digest.is_none());
        assert_eq!(packed.chunks.len(), 1);
        assert!(store.has_blob(&packed.chunks[0]));
    }

    #[test]
    fn pack_expands_a_file_glob_without_packing_the_directory() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/nested/app.js"), b"app");
        write_file(&project.path().join("dist/nested/app.css"), b"css");

        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist/**/*.js".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "dist/nested/app.js");
        assert_eq!(entries[0].digest, Some(Digest::of_bytes(b"app")));
        let mut archived = Vec::new();
        store.get_blob(&packed.chunks[0], &mut archived).unwrap();
        let archived = zstd::decode_all(&archived[..]).unwrap();
        assert!(
            !archived.windows(3).any(|window| window == b"css"),
            "the file the glob did not select was archived"
        );
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
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
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
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        let before = snapshot(project.path(), "dist");

        fs::remove_dir_all(project.path().join("dist")).unwrap();
        write_file(&project.path().join("dist/stale.txt"), b"stale");
        write_file(&project.path().join("keep.txt"), b"keep");

        restore(project.path(), &entries, &packed.chunks, &store).unwrap();

        assert_eq!(snapshot(project.path(), "dist"), before);
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
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        fs::remove_dir_all(project.path().join("dist")).unwrap();

        let b = entries
            .iter()
            .find(|entry| entry.path == "dist/b.txt")
            .unwrap();
        assert!(b.digest.is_some());
        let blob = store.blob_path(&packed.chunks[0]).unwrap();
        fs::write(&blob, b"not-a-zstd-frame").unwrap();

        let error = restore(project.path(), &entries, &packed.chunks, &store).unwrap_err();
        assert!(error.to_string().contains("archive"), "{error}");
        assert!(
            !project.path().join("dist").exists(),
            "partial restore left dist behind"
        );
        assert_eq!(fs::read(project.path().join("keep.txt")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn restore_leaves_matching_outputs_in_place_and_prunes_stale_ones() {
        use std::os::unix::fs::MetadataExt;
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/same.js"), b"same");
        write_file(&project.path().join("dist/changed.js"), b"new");
        write_file(&project.path().join("dist/nested/kept.js"), b"kept");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        let before = snapshot(project.path(), "dist");

        // The checkout drifted: one file changed, stale files and a stale
        // directory appeared. `same.js` is untouched.
        let inode = |rel: &str| fs::metadata(project.path().join(rel)).unwrap().ino();
        let same_before = inode("dist/same.js");
        fs::write(project.path().join("dist/changed.js"), b"old").unwrap();
        write_file(&project.path().join("dist/stale.js"), b"stale");
        write_file(&project.path().join("dist/gone/deep.js"), b"stale");

        restore(project.path(), &entries, &packed.chunks, &store).unwrap();

        assert_eq!(snapshot(project.path(), "dist"), before);
        assert_eq!(
            inode("dist/same.js"),
            same_before,
            "matching file was rewritten"
        );
    }

    #[test]
    fn restore_replaces_outputs_whose_type_changed() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/was_dir"), b"file now");
        write_file(&project.path().join("dist/was_file/inner.js"), b"dir now");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        let before = snapshot(project.path(), "dist");

        fs::remove_dir_all(project.path().join("dist")).unwrap();
        write_file(&project.path().join("dist/was_dir/child.js"), b"x");
        write_file(&project.path().join("dist/was_file"), b"x");

        restore(project.path(), &entries, &packed.chunks, &store).unwrap();
        assert_eq!(snapshot(project.path(), "dist"), before);
    }

    #[test]
    fn restore_skips_a_running_restores_temp_file_and_prunes_an_old_one() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/app.js"), b"app");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();

        let live = project.path().join("dist/.app.js.aster-restore.1.0");
        let old = project.path().join("dist/.app.js.aster-restore.2.0");
        write_file(&live, b"in flight");
        write_file(&old, b"left by a killed restore");
        let long_ago = std::time::SystemTime::now() - RESTORE_TEMP_GRACE * 2;
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        restore(project.path(), &entries, &packed.chunks, &store).unwrap();
        assert!(live.exists(), "another restore's temp file was removed");
        assert!(!old.exists(), "a stale temp file survived");
    }

    #[test]
    fn restore_refuses_a_missing_blob_before_touching_outputs() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/a.txt"), b"aaa");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        fs::remove_file(store.blob_path(&packed.chunks[0]).unwrap()).unwrap();
        fs::write(project.path().join("dist/a.txt"), b"local").unwrap();

        let error = restore(project.path(), &entries, &packed.chunks, &store).unwrap_err();
        assert!(error.to_string().contains("no blob"), "{error}");
        assert_eq!(
            fs::read(project.path().join("dist/a.txt")).unwrap(),
            b"local"
        );
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
            let error = restore(project.path(), &entries, &[], &store).unwrap_err();
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
        assert!(restore(project.path(), &link, &[], &store).is_err());
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
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        restore(project.path(), &entries, &packed.chunks, &store).unwrap();

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
        let packed = pack(project.path(), &["dist/app.js".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        fs::remove_file(project.path().join("dist/app.js")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), project.path().join("dist/app.js")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), project.path().join("dist/app.js"))
            .unwrap();

        restore(project.path(), &entries, &packed.chunks, &store).unwrap();
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
        let packed = pack(project.path(), &["dist/app.js".to_string()], &store)
            .unwrap()
            .expect("stored");
        let entries = packed.tree.clone();
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        write_file(&outside.path().join("app.js"), b"outside");
        std::os::unix::fs::symlink(outside.path(), project.path().join("dist")).unwrap();

        let error = restore(project.path(), &entries, &packed.chunks, &store).unwrap_err();
        assert!(error.to_string().contains("symlinked directory"), "{error}");
        assert_eq!(fs::read(outside.path().join("app.js")).unwrap(), b"outside");
        assert!(fs::symlink_metadata(project.path().join("dist"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// Bytes zstd cannot shrink, so an archive is forced across chunks.
    fn incompressible(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        let mut bytes = Vec::with_capacity(length + 8);
        while bytes.len() < length {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(length);
        bytes
    }

    #[test]
    fn many_small_files_are_one_chunk_and_round_trip() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        for index in 0..700 {
            write_file(
                &project
                    .path()
                    .join(format!("dist/assets/{}/f{index}.js", index % 20)),
                format!("export const n = {index};\n").repeat(5).as_bytes(),
            );
        }
        fs::create_dir_all(project.path().join("dist/empty")).unwrap();
        write_file(&project.path().join("dist/zero.bin"), b"");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .expect("stored");
        assert_eq!(
            packed.chunks.len(),
            1,
            "700 small files are one blob, not 700"
        );
        let before = snapshot(project.path(), "dist");

        fs::remove_dir_all(project.path().join("dist")).unwrap();
        restore(project.path(), &packed.tree, &packed.chunks, &store).unwrap();
        assert_eq!(snapshot(project.path(), "dist"), before);
        assert!(project.path().join("dist/empty").is_dir());
        assert_eq!(fs::read(project.path().join("dist/zero.bin")).unwrap(), b"");
    }

    #[test]
    fn a_file_larger_than_a_chunk_spans_chunks_and_round_trips() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let big = incompressible(super::super::chunks::CHUNK_BYTES + 5_000_000, 7);
        write_file(&project.path().join("out/before.txt"), b"first");
        write_file(&project.path().join("out/big.bin"), &big);
        write_file(&project.path().join("out/after.txt"), b"last");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["out".to_string()], &store)
            .unwrap()
            .expect("stored");
        assert_eq!(packed.chunks.len(), 2, "a file past 32 MB spans two chunks");

        fs::remove_dir_all(project.path().join("out")).unwrap();
        restore(project.path(), &packed.tree, &packed.chunks, &store).unwrap();
        assert_eq!(fs::read(project.path().join("out/big.bin")).unwrap(), big);
        assert_eq!(
            fs::read(project.path().join("out/after.txt")).unwrap(),
            b"last"
        );
    }

    #[test]
    fn the_same_outputs_pack_to_the_same_chunks() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/a.js"), b"a");
        write_file(&project.path().join("dist/b.js"), b"b");
        let store = LocalStore::new(cache.path().to_path_buf());
        let first = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .unwrap();
        // A later mtime must not change the archive.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_file(&project.path().join("dist/a.js"), b"a");
        let second = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .unwrap();
        assert_eq!(first.chunks, second.chunks);
    }

    #[test]
    fn a_tree_of_directories_and_links_has_no_archive() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("dist/empty")).unwrap();
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .unwrap();
        assert!(packed.chunks.is_empty());
        let (outputs, archive) = packed.clone().into_manifest_parts();
        assert!(archive.is_none());
        assert_eq!(outputs, packed.tree);
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        restore(project.path(), &packed.tree, &packed.chunks, &store).unwrap();
        assert!(project.path().join("dist/empty").is_dir());
    }

    #[test]
    fn a_tree_that_already_matches_never_opens_the_archive() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/a.js"), b"a");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .unwrap();
        // If restore read the archive, this would fail it.
        fs::write(store.blob_path(&packed.chunks[0]).unwrap(), b"garbage").unwrap();
        restore(project.path(), &packed.tree, &packed.chunks, &store).unwrap();
        assert_eq!(fs::read(project.path().join("dist/a.js")).unwrap(), b"a");
    }

    #[test]
    fn bytes_that_do_not_match_the_entry_digest_are_a_miss_not_a_partial_tree() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/a.js"), b"aaa");
        write_file(&project.path().join("dist/b.js"), b"bbb");
        write_file(&project.path().join("keep.txt"), b"keep");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .unwrap();
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        // The manifest says b.js holds other bytes than the archive does.
        let mut tree = packed.tree.clone();
        tree.iter_mut()
            .find(|entry| entry.path == "dist/b.js")
            .unwrap()
            .digest = Some(Digest::of_bytes(b"something else"));

        let error = restore(project.path(), &tree, &packed.chunks, &store).unwrap_err();
        assert!(format!("{error:#}").contains("digest"), "{error:#}");
        assert!(
            !project.path().join("dist").exists(),
            "a.js was left behind"
        );
        assert_eq!(fs::read(project.path().join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn an_archive_missing_a_file_is_a_miss() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_file(&project.path().join("dist/a.js"), b"aaa");
        let store = LocalStore::new(cache.path().to_path_buf());
        let packed = pack(project.path(), &["dist".to_string()], &store)
            .unwrap()
            .unwrap();
        fs::remove_dir_all(project.path().join("dist")).unwrap();
        let mut tree = packed.tree.clone();
        tree.push(OutputEntry {
            path: "dist/b.js".into(),
            kind: OutputKind::File,
            digest: Some(Digest::of_bytes(b"bbb")),
            mode: 0o644,
            target: None,
        });
        let error = restore(project.path(), &tree, &packed.chunks, &store).unwrap_err();
        assert!(error.to_string().contains("holds 1 of 2 files"), "{error}");
        assert!(!project.path().join("dist").exists());
    }

    #[test]
    fn a_member_that_names_a_path_is_refused_and_writes_nothing() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let outside = project.path().join("outside.txt");
        let store = LocalStore::new(cache.path().to_path_buf());
        // A hostile archive: the member is named like a path, not an index.
        let mut builder = tar::Builder::new(Vec::new());
        // `append` writes the header as given, so the raw name survives.
        let mut header = tar::Header::new_gnu();
        header.as_old_mut().name[..14].copy_from_slice(b"../outside.txt");
        header.set_size(3);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, &b"bad"[..]).unwrap();
        let archive = zstd::encode_all(&builder.into_inner().unwrap()[..], 1).unwrap();
        let chunk = Digest::of_bytes(&archive);
        store.put_blob(&chunk, &archive[..]).unwrap();
        let entry = OutputEntry {
            path: "dist/a.js".into(),
            kind: OutputKind::File,
            digest: Some(Digest::of_bytes(b"bad")),
            mode: 0o644,
            target: None,
        };

        let error = restore(project.path(), &[entry], &[chunk], &store).unwrap_err();
        assert!(format!("{error:#}").contains("not an index"), "{error:#}");
        assert!(!outside.exists());
        assert!(!project.path().join("dist").exists());
    }
}
