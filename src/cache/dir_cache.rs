//! Directory caches: `aster cache save` and `aster cache restore`.
//!
//! A target's cache entry needs its exact inputs. A tool install, a package
//! store or an incremental build tree is useful when it is merely recent, so
//! these entries are named by the caller and fall back to the newest entry
//! under a key prefix.
//!
//! An entry is one tar stream, zstd-compressed and cut into fixed-size chunks.
//! Each chunk is an ordinary content-addressed blob and the chunk list is an
//! ordinary manifest, so uploads, signed URLs, commit verification and
//! retention are the target cache's own. The tar member names start with the
//! index of a path in the caller's sorted list. Restore maps that index back
//! to the caller's own paths, so an entry cannot name where it is written.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use sha2::{Digest as ShaDigest, Sha256};
use walkdir::WalkDir;

use super::artifact::{Digest, Manifest, OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION};
use super::aster_srv::{repo_from_env, AsterSrvBackend};
use super::backend::{CacheBackend, CacheMode, ASTER_SRV_URL_ENV};
use super::chunks::{run_parallel, with_retries, write_chunked};
use super::hasher::encode_hex;
use crate::errln;

const NAMESPACE: &str = "dir/v1/";
/// The target recorded on an entry. The dashboard shows it; restore checks it.
const ENTRY_TARGET: &str = "//:dir-cache";
const MAX_KEY_LEN: usize = 256;
const ZSTD_LEVEL: i32 = 3;
/// The server hashes every new chunk before it stores the manifest. This stays
/// under the load balancer's 120 second backend timeout.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(110);

/// What to save or restore.
#[derive(Debug, Clone)]
pub struct Spec {
    pub key: String,
    /// Key prefixes, in priority order. Restore only.
    pub restore_keys: Vec<String>,
    pub paths: Vec<String>,
    /// Globs relative to each path. Save only.
    pub excludes: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Saved {
    AlreadyStored,
    NothingToStore,
    NotWritable,
    Stored { chunks: usize },
}

#[derive(Debug, PartialEq, Eq)]
enum Restored {
    Miss,
    Hit { key: String, files: u64 },
}

/// `aster cache save`. Only bad arguments fail: the build this follows passed.
pub fn run_save(spec: &Spec, cwd: &Path) -> Result<()> {
    let identity = Identity::new(&spec.paths, cwd)?;
    let excludes = exclude_set(&spec.excludes)?;
    let stored_key = identity.key(&spec.key)?;
    let mode = CacheMode::from_env()?;
    if !matches!(mode, CacheMode::Write | CacheMode::ReadWrite) {
        errln!("[aster] cache save: skipped, ASTER_CACHE_MODE does not write");
        return Ok(());
    }
    let remote = match open_remote(cwd) {
        Ok(remote) => remote,
        Err(error) => {
            errln!("[aster] Warning: cache save: remote cache unavailable: {error:#}");
            return Ok(());
        }
    };
    let Some(remote) = remote else {
        errln!("[aster] cache save: skipped, no {ASTER_SRV_URL_ENV} and credentials");
        return Ok(());
    };
    match save(&identity, &stored_key, &excludes, &remote) {
        Ok(Saved::AlreadyStored) => errln!("[aster] cache save: {} already stored", spec.key),
        Ok(Saved::NothingToStore) => {
            errln!("[aster] Warning: cache save: none of the paths exist; nothing stored");
        }
        Ok(Saved::NotWritable) => {
            errln!("[aster] cache save: skipped, this identity may not write the cache");
        }
        Ok(Saved::Stored { chunks }) => {
            errln!("[aster] cache save: stored {} ({chunks} chunks)", spec.key);
        }
        Err(error) => errln!("[aster] Warning: cache save failed: {error:#}"),
    }
    Ok(())
}

/// `aster cache restore`. A miss or a remote failure leaves the job to build
/// from nothing. Only a failed extraction is an error: it may have left a
/// partial tree that later steps would trust.
pub fn run_restore(spec: &Spec, cwd: &Path) -> Result<()> {
    let identity = Identity::new(&spec.paths, cwd)?;
    let stored_key = identity.key(&spec.key)?;
    // A blank restore-keys line means nothing, as it does in actions/cache.
    let prefixes = spec
        .restore_keys
        .iter()
        .filter(|prefix| !prefix.trim().is_empty())
        .map(|prefix| identity.key(prefix))
        .collect::<Result<Vec<_>>>()?;
    let remote = if CacheMode::from_env()? == CacheMode::Off {
        None
    } else {
        open_remote(cwd).unwrap_or_else(|error| {
            errln!("[aster] Warning: cache restore: remote cache unavailable: {error:#}");
            None
        })
    };
    let restored = match remote {
        Some(remote) => restore(&identity, &stored_key, &prefixes, &remote)?,
        None => Restored::Miss,
    };
    let matched = match &restored {
        Restored::Hit { key, files } => {
            let user_key = identity.user_key(key);
            errln!("[aster] cache restore: restored {user_key} ({files} entries)");
            Some(user_key.to_string())
        }
        Restored::Miss => {
            errln!("[aster] cache restore: no entry for {}", spec.key);
            None
        }
    };
    write_github_output(&spec.key, matched.as_deref())
}

fn open_remote(cwd: &Path) -> Result<Option<AsterSrvBackend>> {
    let url = std::env::var(ASTER_SRV_URL_ENV).ok();
    let repo = Some(repo_from_env(cwd))
        .filter(|repo| !repo.trim().is_empty())
        .or_else(|| std::env::var("GITHUB_REPOSITORY").ok());
    let Some(remote) = AsterSrvBackend::from_env(url.as_deref(), repo.as_deref())? else {
        return Ok(None);
    };
    let remote = remote.with_write_timeout(COMMIT_TIMEOUT);
    Ok(Some(match std::env::var("ASTER_SRV_GCS_BASE") {
        Ok(base) if !base.trim().is_empty() => remote.redirect_gcs_to(base),
        _ => remote,
    }))
}

fn write_github_output(key: &str, matched: Option<&str>) -> Result<()> {
    let Ok(path) = std::env::var("GITHUB_OUTPUT") else {
        return Ok(());
    };
    if path.is_empty() {
        return Ok(());
    }
    let mut output = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("open {path}"))?;
    let exact = matched == Some(key);
    writeln!(output, "cache-hit={exact}")?;
    writeln!(output, "cache-primary-key={key}")?;
    writeln!(output, "cache-matched-key={}", matched.unwrap_or(""))?;
    Ok(())
}

/// One declared path and where it is on this machine.
#[derive(Debug, Clone)]
struct Root {
    /// A declared glob is rooted at its literal leading directories, which
    /// exist on restore even though the files the glob matched do not.
    path: PathBuf,
    /// What to store under `path`. `None` stores all of it.
    include: Option<GlobMatcher>,
}

impl Root {
    /// A file a glob selects, or anything inside a directory it selects.
    fn selects(&self, relative: &Path) -> bool {
        let Some(include) = &self.include else {
            return true;
        };
        relative
            .ancestors()
            .any(|path| !path.as_os_str().is_empty() && include.is_match(path))
    }

    /// Whether the walk needs to enter `relative`. Without `**` a glob names
    /// a fixed depth, so `~/*.x` does not read all of the home directory.
    fn reaches(&self, relative: &Path) -> bool {
        let Some(include) = &self.include else {
            return true;
        };
        let pattern = include.glob().glob();
        pattern.contains("**")
            || relative.components().count() <= pattern.split('/').count()
            || self.selects(relative)
    }

    /// Whether a glob selects anything here, or a plain path exists.
    fn has_content(&self) -> bool {
        let Ok(start) = fs::canonicalize(&self.path) else {
            return false;
        };
        if self.include.is_none() {
            return true;
        }
        WalkDir::new(&start)
            .into_iter()
            .filter_entry(|entry| {
                self.reaches(entry.path().strip_prefix(&start).unwrap_or(entry.path()))
            })
            .flatten()
            .any(|entry| self.selects(entry.path().strip_prefix(&start).unwrap_or(entry.path())))
    }
}

/// `path`'s literal leading directories and the glob after them, if any.
fn split_glob(path: &str) -> (&str, Option<&str>) {
    // Only these: `[` and `{` are ordinary in real names (`app/[id]`).
    let Some(first) = path.find(['*', '?']) else {
        return (path, None);
    };
    match path[..first].rfind(std::path::is_separator) {
        Some(0) => ("/", Some(&path[1..])),
        Some(slash) => (&path[..slash], Some(&path[slash + 1..])),
        None => ("", Some(path)),
    }
}

/// The sorted roots, and the part of every stored key they determine.
#[derive(Debug)]
struct Identity {
    roots: Vec<Root>,
    key_prefix: String,
}

impl Identity {
    fn new(paths: &[String], cwd: &Path) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        Self::with_home(paths, cwd, home.as_deref())
    }

    fn with_home(paths: &[String], cwd: &Path, home: Option<&Path>) -> Result<Self> {
        let mut declared: Vec<String> = paths.to_vec();
        declared.sort();
        declared.dedup();
        if declared.is_empty() {
            bail!("at least one --path is required");
        }
        let mut roots = Vec::with_capacity(declared.len());
        for path in &declared {
            if path.is_empty() || path.contains('\n') || path.starts_with('!') {
                bail!("--path {path:?} must be a path or glob; use --exclude to leave files out");
            }
            let (literal, pattern) = split_glob(path);
            let include = pattern
                .map(|pattern| {
                    GlobBuilder::new(pattern)
                        .literal_separator(true)
                        .build()
                        .map(|glob| glob.compile_matcher())
                        .with_context(|| format!("--path {path:?}"))
                })
                .transpose()?;
            let resolved = if literal == "~" {
                home.context("~ needs HOME")?.to_path_buf()
            } else if let Some(rest) = literal.strip_prefix("~/") {
                home.context("~/ needs HOME")?.join(rest)
            } else {
                cwd.join(literal)
            };
            roots.push(Root {
                path: resolved,
                include,
            });
        }
        // The declared text, not the resolved path: runners and checkouts at
        // different locations still share entries.
        let mut hasher = Sha256::new();
        for path in &declared {
            ShaDigest::update(&mut hasher, path.as_bytes());
            ShaDigest::update(&mut hasher, b"\n");
        }
        let digest = encode_hex(ShaDigest::finalize(hasher).as_ref());
        Ok(Self {
            roots,
            key_prefix: format!("{NAMESPACE}{}/", &digest[..16]),
        })
    }

    fn key(&self, user_key: &str) -> Result<String> {
        if user_key.is_empty() || user_key.chars().any(char::is_control) {
            bail!("cache key {user_key:?} must be non-empty text without control characters");
        }
        let key = format!("{}{user_key}", self.key_prefix);
        if key.len() > MAX_KEY_LEN {
            bail!(
                "cache key {user_key:?} is too long: at most {} bytes",
                MAX_KEY_LEN - self.key_prefix.len()
            );
        }
        Ok(key)
    }

    fn user_key<'a>(&self, stored: &'a str) -> &'a str {
        stored.strip_prefix(&self.key_prefix).unwrap_or(stored)
    }
}

fn exclude_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("--exclude {pattern:?}"))?);
    }
    builder.build().context("build --exclude set")
}

fn save(
    identity: &Identity,
    stored_key: &str,
    excludes: &GlobSet,
    remote: &AsterSrvBackend,
) -> Result<Saved> {
    if !remote.writable() {
        return Ok(Saved::NotWritable);
    }
    // An empty entry would be a permanent hit: stored keys are not replaced.
    if !identity.roots.iter().any(Root::has_content) {
        return Ok(Saved::NothingToStore);
    }
    if remote.lookup_dir(stored_key, &[])?.as_deref() == Some(stored_key) {
        return Ok(Saved::AlreadyStored);
    }
    let started = Instant::now();
    let digests = upload_archive(&identity.roots, excludes, remote)?;
    if !remote.writable() {
        return Ok(Saved::NotWritable);
    }
    let chunks = digests.len();
    let manifest = entry_manifest(stored_key, digests, started.elapsed());
    // The server keeps each chunk it has hashed, so a commit that outlasts
    // the timeout continues where it stopped.
    with_retries(|| remote.put_manifest(&manifest))?;
    if !remote.writable() {
        return Ok(Saved::NotWritable);
    }
    Ok(Saved::Stored { chunks })
}

fn entry_manifest(key: &str, digests: Vec<Digest>, elapsed: Duration) -> Manifest {
    let outputs = digests
        .into_iter()
        .enumerate()
        .map(|(index, digest)| OutputEntry {
            path: format!("chunk/{index:06}"),
            kind: OutputKind::File,
            digest: Some(digest),
            mode: 0o644,
            target: None,
        })
        .collect();
    Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        key: key.to_string(),
        target: ENTRY_TARGET.to_string(),
        exit_code: 0,
        duration_ms: elapsed.as_millis() as u64,
        created_at: chrono::Utc::now().to_rfc3339(),
        outputs,
        log: None,
        aster_version: env!("CARGO_PKG_VERSION").to_string(),
        packs: Vec::new(),
        archive: None,
    }
}

/// Tar and compress `roots`, uploading each chunk as soon as it is full.
/// Returns the chunk digests in stream order.
fn upload_archive(
    roots: &[Root],
    excludes: &GlobSet,
    remote: &AsterSrvBackend,
) -> Result<Vec<Digest>> {
    write_chunked(
        |digest, bytes| upload_chunk(remote, digest, bytes),
        |sink| pack(roots, excludes, sink).map(drop),
    )
}

fn upload_chunk(remote: &AsterSrvBackend, digest: &Digest, bytes: &[u8]) -> Result<()> {
    let missing = remote.find_missing(std::slice::from_ref(digest))?;
    if !missing.is_empty() {
        remote.put_blob(digest, &mut Cursor::new(bytes))?;
    }
    Ok(())
}

fn pack<W: Write>(roots: &[Root], excludes: &GlobSet, sink: W) -> Result<W> {
    let encoder = zstd::stream::write::Encoder::new(sink, ZSTD_LEVEL).context("start zstd")?;
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    for (index, root) in roots.iter().enumerate() {
        append_root(&mut archive, index, root, excludes)?;
    }
    let encoder = archive.into_inner().context("finish the tar stream")?;
    encoder.finish().context("finish zstd")
}

fn append_root<W: Write>(
    archive: &mut tar::Builder<W>,
    index: usize,
    root: &Root,
    excludes: &GlobSet,
) -> Result<()> {
    // A declared path that is a link stores what it points at: the member
    // for the root is then a directory, which restore can always create.
    let Ok(start) = fs::canonicalize(&root.path) else {
        return Ok(());
    };
    let mut links = FirstLinks::new();
    let walker = WalkDir::new(&start)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            let relative = entry.path().strip_prefix(&start).unwrap_or(entry.path());
            relative.as_os_str().is_empty()
                || (!excludes.is_match(relative) && root.reaches(relative))
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            // A build tool removed it between the listing and the read.
            Err(error) if is_not_found(&error) => continue,
            // A glob's root holds things it did not ask for.
            Err(error) if is_unselected_and_denied(root, &start, &error) => continue,
            Err(error) => return Err(error).context("walk a cache path"),
        };
        let kind = entry.file_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            continue;
        }
        let relative = entry.path().strip_prefix(&start)?;
        if !root.selects(relative) {
            continue;
        }
        let member = member_name(index, relative);
        let appended = if kind.is_file() {
            append_file(archive, entry.path(), &member, &mut links)
        } else {
            archive.append_path_with_name(entry.path(), &member)
        };
        match appended {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("add {}", entry.path().display()));
            }
        }
    }
    Ok(())
}

/// The member that first stored each multiply-linked file under one root.
type FirstLinks = HashMap<(u64, u64), PathBuf>;

/// Add one regular file, writing exactly the size its header states. The tar
/// crate's own file append copies to end of file, so a file that changes
/// during the walk would desynchronise every member after it.
///
/// A file already stored under this root becomes a hard-link member: Cargo
/// links each final artifact into `deps/`, which would otherwise be stored
/// twice and restored as two inodes.
fn append_file<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &Path,
    member: &Path,
    links: &mut FirstLinks,
) -> io::Result<()> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let mut header = tar::Header::new_gnu();
    header.set_metadata(&metadata);
    let id = hard_link_id(&metadata);
    if let Some(first) = id.and_then(|id| links.get(&id)) {
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        return archive.append_link(&mut header, member, first);
    }
    archive.append_data(&mut header, member, ExactFile::new(file, metadata.len()))?;
    if let Some(id) = id {
        links.insert(id, member.to_path_buf());
    }
    Ok(())
}

#[cfg(unix)]
fn hard_link_id(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (metadata.nlink() > 1).then(|| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn hard_link_id(_metadata: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Yields exactly `remaining` bytes, or fails if the file has become shorter.
struct ExactFile<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> ExactFile<R> {
    fn new(inner: R, remaining: u64) -> Self {
        Self { inner, remaining }
    }
}

impl<R: Read> Read for ExactFile<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let limit = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(io::Error::other("a file shrank while it was being stored"));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn is_not_found(error: &walkdir::Error) -> bool {
    error
        .io_error()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// An unreadable directory a glob did not ask for. One inside what it did
/// ask for still fails the save, as a partial entry would look complete.
fn is_unselected_and_denied(root: &Root, start: &Path, error: &walkdir::Error) -> bool {
    let denied = error
        .io_error()
        .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
    let selected = error
        .path()
        .and_then(|path| path.strip_prefix(start).ok())
        .is_none_or(|relative| root.selects(relative));
    root.include.is_some() && denied && !selected
}

fn member_name(index: usize, relative: &Path) -> PathBuf {
    let base = PathBuf::from(index.to_string());
    if relative.as_os_str().is_empty() {
        base
    } else {
        base.join(relative)
    }
}

fn restore(
    identity: &Identity,
    stored_key: &str,
    prefixes: &[String],
    remote: &AsterSrvBackend,
) -> Result<Restored> {
    let found = match remote.lookup_dir(stored_key, prefixes) {
        Ok(found) => found,
        Err(error) => {
            errln!("[aster] Warning: cache restore: lookup failed: {error:#}");
            return Ok(Restored::Miss);
        }
    };
    let Some(key) = found else {
        return Ok(Restored::Miss);
    };
    let digests = match entry_digests(remote, &key) {
        Ok(digests) => digests,
        Err(error) => {
            errln!("[aster] Warning: cache restore: unusable entry: {error:#}");
            return Ok(Restored::Miss);
        }
    };
    // The runner's temp directory is sized for this; `/tmp` may not be.
    let spool = match std::env::var_os("RUNNER_TEMP") {
        Some(dir) if Path::new(&dir).is_dir() => tempfile::tempdir_in(dir),
        _ => tempfile::tempdir(),
    }
    .context("create a restore spool")?;
    let files = match download_entry(remote, &key, &digests, spool.path()) {
        Ok(files) => files,
        Err(error) => {
            errln!("[aster] Warning: cache restore: download failed: {error:#}");
            return Ok(Restored::Miss);
        }
    };
    let entries =
        extract(ChunkFiles::new(files), &identity.roots).context("extract the cache entry")?;
    Ok(Restored::Hit {
        key,
        files: entries,
    })
}

/// The chunk digests of a stored entry, in stream order.
fn entry_digests(remote: &AsterSrvBackend, key: &str) -> Result<Vec<Digest>> {
    let manifest = remote
        .get_manifest(key)?
        .context("the lookup hit has no manifest")?;
    if manifest.target != ENTRY_TARGET {
        bail!("{key} is not a directory-cache entry");
    }
    let mut digests = Vec::with_capacity(manifest.outputs.len());
    for (index, output) in manifest.outputs.iter().enumerate() {
        if output.path != format!("chunk/{index:06}") || output.kind != OutputKind::File {
            bail!("{key} has an unexpected chunk at {index}");
        }
        let digest = output.digest.clone().context("a chunk has no digest")?;
        if !remote.has_blob(&digest) {
            bail!("{key} names a chunk the cache no longer has");
        }
        digests.push(digest);
    }
    if digests.is_empty() {
        bail!("{key} has no chunks");
    }
    Ok(digests)
}

/// Fetch every chunk of `key` into `dir`, checking each digest. Returns the
/// files in stream order. Nothing is extracted until all of them have arrived,
/// so a failed download leaves the destination untouched.
///
/// A signed URL may have as little as five minutes left when lookup returns
/// it, which a large entry on a slow link can outlast. Each pass that made
/// progress asks for fresh URLs and continues with the chunks still missing.
fn download_entry(
    remote: &AsterSrvBackend,
    key: &str,
    digests: &[Digest],
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = (0..digests.len())
        .map(|index| dir.join(format!("chunk-{index:06}")))
        .collect();
    let done: Vec<AtomicBool> = digests.iter().map(|_| AtomicBool::new(false)).collect();
    let finished = |done: &[AtomicBool]| {
        done.iter()
            .filter(|chunk| chunk.load(Ordering::Acquire))
            .count()
    };
    loop {
        let before = finished(&done);
        let Err(error) = download_pending(remote, digests, &paths, &done) else {
            return Ok(paths);
        };
        if finished(&done) == before {
            return Err(error);
        }
        if remote.lookup_dir(key, &[])?.as_deref() != Some(key) {
            return Err(error);
        }
    }
}

/// One pass over the chunks not yet in `done`. Stops at the first failure.
fn download_pending(
    remote: &AsterSrvBackend,
    digests: &[Digest],
    paths: &[PathBuf],
    done: &[AtomicBool],
) -> Result<()> {
    run_parallel(
        digests.len(),
        |index| done[index].load(Ordering::Acquire),
        |index| {
            with_retries(|| download_chunk(remote, &digests[index], &paths[index]))?;
            done[index].store(true, Ordering::Release);
            Ok(())
        },
    )
}

fn download_chunk(remote: &AsterSrvBackend, digest: &Digest, path: &Path) -> Result<()> {
    let mut file = File::create(path).context("create a spool file")?;
    if !remote.get_blob(digest, &mut file)? {
        bail!("no download URL for chunk {}", digest.as_str());
    }
    Ok(())
}

/// Reads spooled chunks in order and removes each one once it has been read,
/// so a restore holds about one extra copy of the compressed entry at most.
struct ChunkFiles {
    pending: VecDeque<PathBuf>,
    current: Option<(PathBuf, File)>,
}

impl ChunkFiles {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            pending: paths.into(),
            current: None,
        }
    }
}

impl Read for ChunkFiles {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.current.is_none() {
                let Some(path) = self.pending.pop_front() else {
                    return Ok(0);
                };
                let file = File::open(&path)?;
                self.current = Some((path, file));
            }
            let Some((path, file)) = self.current.as_mut() else {
                return Ok(0);
            };
            let read = file.read(buffer)?;
            if read > 0 {
                return Ok(read);
            }
            fs::remove_file(path.as_path())?;
            self.current = None;
        }
    }
}

/// Unpack a compressed tar stream into `roots`. Returns the entries written.
fn extract<R: Read>(reader: R, roots: &[Root]) -> Result<u64> {
    let decoder = zstd::stream::read::Decoder::new(reader).context("start zstd")?;
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    let mut unpacker = Unpacker::default();
    let mut count = 0;
    for entry in archive.entries().context("read the tar stream")? {
        let mut entry = entry.context("read a tar entry")?;
        let member = entry.path().context("read a tar path")?.into_owned();
        unpacker.unpack(&mut entry, &member, roots)?;
        count += 1;
    }
    unpacker.finish()?;
    Ok(count)
}

/// Writes tar members under their roots and refuses to follow a symlink out.
#[derive(Default)]
struct Unpacker {
    /// Directories already checked to be real directories under a root.
    checked: HashSet<PathBuf>,
    /// Directory modes and mtimes, applied last: a read-only directory (the
    /// Go module cache has them) could not receive its own files otherwise,
    /// and writing a directory's children changes its mtime.
    /// The flag marks a declared root, whose metadata is best effort.
    directories: Vec<(PathBuf, u32, u64, bool)>,
}

impl Unpacker {
    fn unpack<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        member: &Path,
        roots: &[Root],
    ) -> Result<()> {
        let (root, relative) = locate(member, roots)?;
        let destination = root.path.join(&relative);
        let is_root = relative.as_os_str().is_empty();
        let destination = if is_root {
            root.path.clone()
        } else {
            destination
        };
        self.prepare_parents(root, &relative)?;
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                if !is_root {
                    self.refuse_symlink(&destination)?;
                }
                fs::create_dir_all(&destination)
                    .with_context(|| format!("create {}", destination.display()))?;
                // A root may be the caller's and not ours to change (`~`, or
                // a system directory); writing into it decides.
                if let (Err(error), false) = (make_writable(&destination), is_root) {
                    return Err(error);
                }
                self.checked.insert(destination.clone());
                let mode = entry.header().mode().context("read a directory mode")?;
                let mtime = entry.header().mtime().context("read a directory mtime")?;
                self.directories.push((destination, mode, mtime, is_root));
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                entry
                    .unpack(&destination)
                    .with_context(|| format!("write {}", destination.display()))?;
            }
            tar::EntryType::Symlink => {
                self.checked.remove(&destination);
                entry
                    .unpack(&destination)
                    .with_context(|| format!("link {}", destination.display()))?;
            }
            tar::EntryType::Link => {
                let first = entry
                    .link_name()
                    .context("read a hard link's target")?
                    .context("a hard link names no target")?
                    .into_owned();
                self.link(root, &first, &destination, roots)?;
            }
            other => bail!("{} has unsupported type {other:?}", member.display()),
        }
        Ok(())
    }

    /// Make `destination` another name for the earlier member `first`, which
    /// must be a regular file under the same root.
    fn link(
        &mut self,
        root: &Root,
        first: &Path,
        destination: &Path,
        roots: &[Root],
    ) -> Result<()> {
        let (first_root, relative) = locate(first, roots)?;
        if !std::ptr::eq(first_root, root) || relative.as_os_str().is_empty() {
            bail!("hard link target {} is outside its root", first.display());
        }
        self.prepare_parents(root, &relative)?;
        let source = root.path.join(&relative);
        let is_file = fs::symlink_metadata(&source)
            .with_context(|| format!("inspect {}", source.display()))?
            .is_file();
        if !is_file || source == destination {
            bail!("hard link target {} is not a file", first.display());
        }
        match fs::remove_file(destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("replace {}", destination.display()));
            }
        }
        if fs::hard_link(&source, destination).is_err() {
            // A destination filesystem without hard links still gets the bytes.
            fs::copy(&source, destination)
                .with_context(|| format!("link {}", destination.display()))?;
        }
        Ok(())
    }

    fn refuse_symlink(&self, path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "{} is a symlink; refusing to write through it",
                    path.display()
                )
            }
            _ => Ok(()),
        }
    }

    /// Create the parents of a member. The root's own ancestors belong to the
    /// caller; below the root, every component must be a real directory.
    fn prepare_parents(&mut self, root: &Root, relative: &Path) -> Result<()> {
        if relative.as_os_str().is_empty() {
            if let Some(parent) = root.path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            return Ok(());
        }
        fs::create_dir_all(&root.path)
            .with_context(|| format!("create {}", root.path.display()))?;
        let mut current = root.path.clone();
        let parents = relative.parent().unwrap_or(Path::new(""));
        for component in parents.components() {
            current.push(component);
            if self.checked.contains(&current) {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "{} is a symlink; refusing to write through it",
                        current.display()
                    );
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => bail!("{} is not a directory", current.display()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current)
                        .with_context(|| format!("create {}", current.display()))?;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("inspect {}", current.display()));
                }
            }
            self.checked.insert(current.clone());
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        // Children first, so a read-only parent cannot block its children.
        for (path, mode, mtime, is_root) in self.directories.iter().rev() {
            let restored = restore_directory(path, *mode, *mtime);
            if !is_root {
                restored?;
            }
        }
        Ok(())
    }
}

/// A directory left read-only by an earlier restore (the Go module cache has
/// them) must accept its files again. Its stored mode is applied at the end.
#[cfg(unix)]
fn make_writable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .with_context(|| format!("inspect {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))
            .with_context(|| format!("make {} writable", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_writable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restore_directory(path: &Path, mode: u32, mtime: u64) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    File::open(path)
        .and_then(|directory| {
            directory.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime))
        })
        .with_context(|| format!("set the mtime of {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
        .with_context(|| format!("set the mode of {}", path.display()))
}

#[cfg(not(unix))]
fn restore_directory(_path: &Path, _mode: u32, _mtime: u64) -> Result<()> {
    Ok(())
}

/// Split a member name into its root and its path under that root. Only plain
/// components are accepted, so a member cannot climb out with `..`.
fn locate<'a>(member: &Path, roots: &'a [Root]) -> Result<(&'a Root, PathBuf)> {
    let mut components = member.components();
    let index = match components.next() {
        Some(Component::Normal(first)) => first
            .to_str()
            .and_then(|text| text.parse::<usize>().ok())
            .with_context(|| format!("unexpected tar member {}", member.display()))?,
        _ => bail!("unexpected tar member {}", member.display()),
    };
    let root = roots
        .get(index)
        .with_context(|| format!("tar member {} names no path", member.display()))?;
    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative.push(part),
            _ => bail!("unsafe tar member {}", member.display()),
        }
    }
    Ok((root, relative))
}

/// What must hold on every platform, Windows included. `tests` below covers
/// the Unix-only parts: modes, symlinks and hard links.
#[cfg(test)]
mod portable {
    use super::*;

    fn roots(paths: &[&str], cwd: &Path) -> Vec<Root> {
        let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        Identity::with_home(&paths, cwd, Some(cwd)).unwrap().roots
    }

    #[test]
    fn a_tree_a_glob_and_a_home_path_round_trip() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("tree/sub/incremental")).unwrap();
        fs::create_dir_all(source.path().join("tree/empty")).unwrap();
        fs::write(source.path().join("tree/sub/file"), b"nested").unwrap();
        fs::write(source.path().join("tree/sub/incremental/skip"), b"no").unwrap();
        fs::create_dir_all(source.path().join("tools")).unwrap();
        fs::write(source.path().join("tools/esbuild-1"), b"tool").unwrap();
        fs::write(source.path().join("tools/other"), b"no").unwrap();
        fs::write(source.path().join("in-home"), b"home").unwrap();
        let paths = ["tree", "tools/esbuild-*", "~/in-home"];
        let excludes = exclude_set(&["**/incremental".to_string()]).unwrap();
        let bytes = pack(&roots(&paths, source.path()), &excludes, Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        extract(&bytes[..], &roots(&paths, target.path())).unwrap();
        let read = |path: &str| fs::read(target.path().join(path)).unwrap();
        assert_eq!(read("tree/sub/file"), b"nested");
        assert!(target.path().join("tree/empty").is_dir());
        assert!(!target.path().join("tree/sub/incremental").exists());
        assert_eq!(read("tools/esbuild-1"), b"tool");
        assert!(!target.path().join("tools/other").exists());
        assert_eq!(read("in-home"), b"home");
    }

    #[test]
    fn a_restore_replaces_what_is_there_and_a_climbing_member_is_refused() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("tree")).unwrap();
        fs::write(source.path().join("tree/file"), b"new").unwrap();
        let excludes = exclude_set(&[]).unwrap();
        let bytes = pack(&roots(&["tree"], source.path()), &excludes, Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        fs::create_dir(target.path().join("tree")).unwrap();
        fs::write(target.path().join("tree/file"), b"old").unwrap();
        extract(&bytes[..], &roots(&["tree"], target.path())).unwrap();
        assert_eq!(fs::read(target.path().join("tree/file")).unwrap(), b"new");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let name = b"0/../../escaped";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        let climbing = zstd::encode_all(&builder.into_inner().unwrap()[..], 1).unwrap();
        assert!(extract(&climbing[..], &roots(&["tree"], target.path())).is_err());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::chunks::lock;
    use super::*;
    use crate::test_results::upload::Credentials;
    use base64::Engine as _;
    use serde_json::Value;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::sync::{Arc, Mutex};
    use std::thread;

    const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    fn identity(paths: &[&str], cwd: &Path) -> Identity {
        let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        Identity::with_home(&paths, cwd, Some(Path::new("/home/ci"))).unwrap()
    }

    fn compressed_tar(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        zstd::encode_all(&builder.into_inner().unwrap()[..], 1).unwrap()
    }

    /// A tar with a raw name: `tar::Builder::append_data` refuses `..`.
    fn raw_file(name: &str, body: &[u8]) -> Vec<u8> {
        compressed_tar(|builder| {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, body).unwrap();
        })
    }

    /// A stand-in for aster-srv and GCS, enough to drive save and restore.
    #[derive(Default)]
    struct Remote {
        objects: HashMap<String, Vec<u8>>,
        manifests: HashMap<String, Vec<u8>>,
        generation: usize,
        /// Downloads the first batch of URLs serves before it expires.
        first_urls_serve: Option<usize>,
        first_url_gets: usize,
        refuse_downloads: bool,
        commit_failures: usize,
        commits: usize,
        lookups: usize,
    }

    fn serve(remote: Arc<Mutex<Remote>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = base.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (remote, base) = (Arc::clone(&remote), base.clone());
                thread::spawn(move || answer(stream, &remote, &base));
            }
        });
        url
    }

    fn answer(mut stream: TcpStream, remote: &Mutex<Remote>, base: &str) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).unwrap();
        let mut parts = request.split_whitespace();
        let (method, path) = (parts.next().unwrap(), parts.next().unwrap());
        let (status, reply) = route(method, path, &body, &mut lock(remote), base);
        write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.len()
        )
        .unwrap();
        stream.write_all(&reply).unwrap();
    }

    fn route(
        method: &str,
        path: &str,
        body: &[u8],
        remote: &mut Remote,
        base: &str,
    ) -> (u16, Vec<u8>) {
        let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let json = |value: Value| (200, serde_json::to_vec(&value).unwrap());
        match (method, path) {
            ("POST", "/v1/cache/dir-lookup") => {
                remote.lookups += 1;
                remote.generation += 1;
                let key = request["key"].as_str().unwrap();
                let Some(bytes) = remote.manifests.get(key) else {
                    return json(serde_json::json!({ "hit": null }));
                };
                let blobs: serde_json::Map<String, Value> = remote
                    .objects
                    .keys()
                    .map(|digest| {
                        let url = format!("{base}/g{}/{digest}", remote.generation);
                        (digest.clone(), Value::String(url))
                    })
                    .collect();
                json(serde_json::json!({
                    "hit": { "key": key, "manifest": BASE64.encode(bytes), "blobs": blobs }
                }))
            }
            ("POST", "/v1/cache/upload") => {
                let mut missing = serde_json::Map::new();
                for digest in request["digests"].as_array().unwrap() {
                    let digest = digest.as_str().unwrap();
                    let url = format!("{base}/g0/{digest}");
                    missing.insert(digest.to_string(), Value::String(url));
                }
                json(serde_json::json!({ "missing": missing }))
            }
            ("POST", "/v1/cache/commit") => {
                remote.commits += 1;
                if remote.commit_failures > 0 {
                    remote.commit_failures -= 1;
                    return (503, b"{}".to_vec());
                }
                let key = request["key"].as_str().unwrap().to_string();
                let manifest = request["manifest"].as_str().unwrap();
                remote
                    .manifests
                    .insert(key, BASE64.decode(manifest).unwrap());
                json(serde_json::json!({ "stored": true }))
            }
            ("PUT", _) => {
                let digest = path.rsplit('/').next().unwrap().to_string();
                remote.objects.insert(digest, body.to_vec());
                (200, Vec::new())
            }
            ("GET", _) => {
                if remote.refuse_downloads {
                    return (403, Vec::new());
                }
                if path.starts_with("/g1/") {
                    if let Some(allowed) = remote.first_urls_serve {
                        remote.first_url_gets += 1;
                        if remote.first_url_gets > allowed {
                            return (403, Vec::new());
                        }
                    }
                }
                let digest = path.rsplit('/').next().unwrap();
                (200, remote.objects[digest].clone())
            }
            _ => (404, Vec::new()),
        }
    }

    fn backend(url: &str) -> AsterSrvBackend {
        AsterSrvBackend::from_parts(
            Some(url),
            Some("acme/app"),
            Some(Credentials::Static("token".to_string())),
            None,
            None,
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn a_download_outlasting_its_signed_urls_asks_for_fresh_ones_and_continues() {
        let state = Arc::new(Mutex::new(Remote {
            first_urls_serve: Some(1),
            ..Remote::default()
        }));
        let bodies: Vec<Vec<u8>> = (0..3).map(|n| vec![n as u8; 64]).collect();
        let digests: Vec<Digest> = bodies.iter().map(|body| Digest::of_bytes(body)).collect();
        {
            let mut remote = lock(&state);
            for (digest, body) in digests.iter().zip(&bodies) {
                remote
                    .objects
                    .insert(digest.as_str().to_string(), body.clone());
            }
            let manifest = entry_manifest("dir/v1/x/k", digests.clone(), Duration::ZERO);
            remote
                .manifests
                .insert("dir/v1/x/k".to_string(), manifest.encode());
        }
        let remote = backend(&serve(Arc::clone(&state)));
        remote.lookup_dir("dir/v1/x/k", &[]).unwrap();

        let spool = tempfile::tempdir().unwrap();
        let files = download_entry(&remote, "dir/v1/x/k", &digests, spool.path()).unwrap();
        for (file, body) in files.iter().zip(&bodies) {
            assert_eq!(&fs::read(file).unwrap(), body);
        }
        assert_eq!(lock(&state).lookups, 2);
    }

    #[test]
    fn a_save_retries_its_commit_and_a_failed_download_leaves_the_tree_alone() {
        let state = Arc::new(Mutex::new(Remote {
            commit_failures: 1,
            ..Remote::default()
        }));
        let remote = backend(&serve(Arc::clone(&state)));
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("tree")).unwrap();
        fs::write(source.path().join("tree/file"), b"cached").unwrap();
        let writer = identity(&["tree"], source.path());
        let key = writer.key("k").unwrap();
        let saved = save(&writer, &key, &exclude_set(&[]).unwrap(), &remote).unwrap();
        assert_eq!(saved, Saved::Stored { chunks: 1 });
        assert_eq!(lock(&state).commits, 2);

        lock(&state).refuse_downloads = true;
        let target = tempfile::tempdir().unwrap();
        let reader = identity(&["tree"], target.path());
        let restored = restore(&reader, &key, &[], &remote).unwrap();
        assert_eq!(restored, Restored::Miss);
        assert!(!target.path().join("tree").exists());

        lock(&state).refuse_downloads = false;
        let restored = restore(&reader, &key, &[], &remote).unwrap();
        assert!(matches!(restored, Restored::Hit { .. }));
        assert_eq!(
            fs::read(target.path().join("tree/file")).unwrap(),
            b"cached"
        );
    }

    #[test]
    fn the_key_depends_on_the_paths_but_not_their_order_or_location() {
        let first = identity(&["b", "a"], Path::new("/one"));
        let same = identity(&["a", "b"], Path::new("/two"));
        let other = identity(&["a"], Path::new("/one"));
        assert_eq!(first.key("k").unwrap(), same.key("k").unwrap());
        assert_ne!(first.key("k").unwrap(), other.key("k").unwrap());
        assert!(first.key("k").unwrap().starts_with(NAMESPACE));
        assert_eq!(first.user_key(&first.key("k").unwrap()), "k");
    }

    #[test]
    fn home_paths_expand_and_globs_and_bad_keys_are_refused() {
        let found = identity(&["~/go/pkg", "rel"], Path::new("/work"));
        assert_eq!(found.roots[0].path, Path::new("/work/rel"));
        assert_eq!(found.roots[1].path, Path::new("/home/ci/go/pkg"));
        let negated = ["!deps".to_string()];
        assert!(Identity::with_home(&negated, Path::new("/w"), None).is_err());
        let invalid = ["deps/*[".to_string()];
        assert!(Identity::with_home(&invalid, Path::new("/w"), None).is_err());
        assert!(found.key("").is_err());
        assert!(found.key("a\nb").is_err());
        assert!(found.key(&"x".repeat(300)).is_err());
    }

    #[test]
    fn a_tree_round_trips_with_modes_mtimes_symlinks_and_excludes() {
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("tree");
        fs::create_dir_all(tree.join("sub/incremental")).unwrap();
        fs::write(tree.join("sub/tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(tree.join("sub/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(tree.join("sub/incremental/skip"), b"no").unwrap();
        symlink("sub/tool", tree.join("link")).unwrap();
        let old = std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::open(tree.join("sub/tool"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        // A read-only directory must still receive its own files.
        fs::create_dir(tree.join("sealed")).unwrap();
        fs::write(tree.join("sealed/file"), b"go module").unwrap();
        fs::set_permissions(tree.join("sealed"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::write(source.path().join("single"), b"one file").unwrap();

        let roots = identity(&["tree", "single", "absent"], source.path()).roots;
        let excludes = exclude_set(&["**/incremental".to_string()]).unwrap();
        let bytes = pack(&roots, &excludes, Vec::new()).unwrap();
        fs::set_permissions(tree.join("sealed"), fs::Permissions::from_mode(0o755)).unwrap();

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree", "single", "absent"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        let tree = target.path().join("tree");
        assert_eq!(fs::read(tree.join("sub/tool")).unwrap(), b"#!/bin/sh\n");
        let tool = fs::metadata(tree.join("sub/tool")).unwrap();
        assert_eq!(tool.mode() & 0o777, 0o755);
        assert_eq!(tool.mtime(), 1_600_000_000);
        assert_eq!(
            fs::read_link(tree.join("link")).unwrap(),
            Path::new("sub/tool")
        );
        assert!(!tree.join("sub/incremental").exists());
        assert_eq!(fs::read(tree.join("sealed/file")).unwrap(), b"go module");
        assert_eq!(
            fs::metadata(tree.join("sealed")).unwrap().mode() & 0o777,
            0o555
        );
        assert_eq!(fs::read(target.path().join("single")).unwrap(), b"one file");
        assert!(!target.path().join("absent").exists());
        fs::set_permissions(tree.join("sealed"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_file_that_shrinks_while_stored_is_an_error_and_one_that_grows_is_cut() {
        let mut shrunk = Vec::new();
        let error = ExactFile::new(&b"ab"[..], 5)
            .read_to_end(&mut shrunk)
            .unwrap_err();
        assert!(error.to_string().contains("shrank"), "{error}");
        let mut grown = Vec::new();
        ExactFile::new(&b"abcdefg"[..], 3)
            .read_to_end(&mut grown)
            .unwrap();
        assert_eq!(grown, b"abc");
    }

    #[test]
    fn a_declared_path_that_is_a_link_restores_as_a_directory() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("real/sub")).unwrap();
        fs::write(source.path().join("real/sub/file"), b"through the link").unwrap();
        symlink("real", source.path().join("alias")).unwrap();
        let roots = identity(&["alias"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["alias"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        assert_eq!(
            fs::read(target.path().join("alias/sub/file")).unwrap(),
            b"through the link"
        );
        assert!(!fs::symlink_metadata(target.path().join("alias"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_restore_replaces_files_in_a_tree_an_earlier_restore_left_read_only() {
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("tree");
        fs::create_dir_all(tree.join("sealed")).unwrap();
        fs::write(tree.join("sealed/file"), b"new").unwrap();
        fs::set_permissions(tree.join("sealed"), fs::Permissions::from_mode(0o555)).unwrap();
        let roots = identity(&["tree"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();
        fs::set_permissions(tree.join("sealed"), fs::Permissions::from_mode(0o755)).unwrap();

        let target = tempfile::tempdir().unwrap();
        let sealed = target.path().join("tree/sealed");
        fs::create_dir_all(&sealed).unwrap();
        fs::write(sealed.join("file"), b"old").unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o555)).unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        assert_eq!(fs::read(sealed.join("file")).unwrap(), b"new");
        assert_eq!(fs::metadata(&sealed).unwrap().mode() & 0o777, 0o555);
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_member_cannot_climb_out_of_its_root() {
        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        let bytes = raw_file("0/../../escaped", b"bad");
        assert!(extract(&bytes[..], &roots).is_err());
        assert!(!target.path().join("escaped").exists());
    }

    #[test]
    fn a_member_cannot_write_through_a_symlink_from_the_same_entry() {
        let outside = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        let bytes = compressed_tar(|builder| {
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            link.set_mode(0o777);
            builder
                .append_link(&mut link, "0/out", outside.path())
                .unwrap();
            let mut file = tar::Header::new_gnu();
            file.set_size(3);
            file.set_mode(0o644);
            file.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut file, "0/out/escaped", &b"bad"[..])
                .unwrap();
        });
        let error = extract(&bytes[..], &roots).unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert!(!outside.path().join("escaped").exists());
    }

    #[test]
    fn a_member_naming_no_declared_path_is_refused() {
        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        for name in ["7/file", "notanumber/file"] {
            assert!(
                extract(&raw_file(name, b"bad")[..], &roots).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn chunk_files_concatenate_in_order_and_are_removed_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a");
        let second = dir.path().join("b");
        fs::write(&first, b"hello ").unwrap();
        fs::write(&second, b"world").unwrap();
        let mut text = String::new();
        ChunkFiles::new(vec![first.clone(), second.clone()])
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello world");
        assert!(!first.exists() && !second.exists());
    }

    #[test]
    fn hard_links_are_stored_once_and_restored_as_one_inode() {
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("target/debug");
        fs::create_dir_all(tree.join("deps")).unwrap();
        // Random-ish bytes: compression must not hide a second copy.
        let body: Vec<u8> = (0..4_000_000_u32)
            .flat_map(|n| n.wrapping_mul(2_654_435_761).to_le_bytes())
            .collect();
        fs::write(tree.join("deps/tool-abc"), &body).unwrap();
        fs::hard_link(tree.join("deps/tool-abc"), tree.join("tool")).unwrap();

        let roots = identity(&["target"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();
        assert!(
            bytes.len() < body.len() + body.len() / 4,
            "{} bytes for one {} byte file",
            bytes.len(),
            body.len()
        );

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["target"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        let debug = target.path().join("target/debug");
        assert_eq!(fs::read(debug.join("tool")).unwrap(), body);
        assert_eq!(
            fs::metadata(debug.join("tool")).unwrap().ino(),
            fs::metadata(debug.join("deps/tool-abc")).unwrap().ino()
        );
    }

    fn hard_link_member(name: &str, first: &str) -> impl FnOnce(&mut tar::Builder<Vec<u8>>) {
        let (name, first) = (name.to_string(), first.to_string());
        move |builder| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Link);
            header.set_size(0);
            header.set_mode(0o644);
            builder.append_link(&mut header, name, first).unwrap();
        }
    }

    #[test]
    fn a_hard_link_cannot_reach_another_root_a_symlink_or_climb_out() {
        let target = tempfile::tempdir().unwrap();
        let outside = target.path().join("outside");
        fs::write(&outside, b"secret").unwrap();
        fs::create_dir_all(target.path().join("a")).unwrap();
        fs::create_dir_all(target.path().join("b")).unwrap();
        fs::write(target.path().join("b/file"), b"other root").unwrap();
        // The same relative name in the member's own root: a link must not
        // quietly use it in place of the root the archive named.
        fs::write(target.path().join("a/file"), b"own root").unwrap();
        symlink(&outside, target.path().join("a/out")).unwrap();
        symlink(target.path(), target.path().join("a/up")).unwrap();
        let roots = identity(&["a", "b"], target.path()).roots;
        for first in ["1/file", "0/out", "0/up/outside", "0"] {
            let bytes = compressed_tar(hard_link_member("0/stolen", first));
            assert!(extract(&bytes[..], &roots).is_err(), "{first}");
            assert!(!target.path().join("a/stolen").exists(), "{first}");
        }
        // The tar crate refuses to write `..` into a link name, so set it raw.
        let bytes = compressed_tar(|builder| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Link);
            header.set_size(0);
            header.set_path("0/stolen").unwrap();
            let name = b"0/../outside";
            header.as_old_mut().linkname[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, io::empty()).unwrap();
        });
        assert!(extract(&bytes[..], &roots).is_err());
        assert!(!target.path().join("a/stolen").exists());
    }

    #[test]
    fn a_hard_link_to_itself_is_refused_and_leaves_the_file() {
        let target = tempfile::tempdir().unwrap();
        fs::create_dir(target.path().join("a")).unwrap();
        fs::write(target.path().join("a/file"), b"keep").unwrap();
        let roots = identity(&["a"], target.path()).roots;
        let bytes = compressed_tar(hard_link_member("0/file", "0/file"));
        assert!(extract(&bytes[..], &roots).is_err());
        assert_eq!(fs::read(target.path().join("a/file")).unwrap(), b"keep");
    }

    /// `/dev/zero` reports length 0 and never ends: the same shape as a file
    /// that grows while it is stored. Without the limit the member would run
    /// past its header's size and this sink would refuse it.
    #[test]
    fn a_file_longer_than_its_header_says_is_cut_at_the_stated_size() {
        struct Capped(usize);
        impl Write for Capped {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0 = self
                    .0
                    .checked_sub(bytes.len())
                    .ok_or_else(|| io::Error::other("wrote past the stated size"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut archive = tar::Builder::new(Capped(1 << 20));
        let member = Path::new("0/grown");
        append_file(
            &mut archive,
            Path::new("/dev/zero"),
            member,
            &mut FirstLinks::new(),
        )
        .unwrap();
    }

    #[test]
    fn an_unreadable_directory_a_glob_selected_fails_the_save() {
        let source = tempfile::tempdir().unwrap();
        let locked = source.path().join("home/data");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let roots = identity(&["home/da*"], source.path()).roots;
        let packed = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(packed.is_err());
    }

    #[test]
    fn a_directory_keeps_its_mtime_after_its_children_are_written() {
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/file"), b"x").unwrap();
        let old = std::time::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        File::open(tree.join("sub"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let roots = identity(&["tree"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        let restored = fs::metadata(target.path().join("tree/sub")).unwrap();
        assert_eq!(restored.mtime(), 1_500_000_000);
    }

    #[test]
    fn a_glob_path_stores_only_its_matches_and_restores_under_its_literal_root() {
        let source = tempfile::tempdir().unwrap();
        for app in ["web", "api"] {
            let build = source.path().join(format!("src/{app}/_build/dev"));
            fs::create_dir_all(build.join("dialyxir_erlang.plt")).unwrap();
            fs::write(build.join("dialyxir_erlang.plt/plt"), app).unwrap();
            fs::write(build.join("other"), b"not stored").unwrap();
        }
        let paths = ["src/*/_build/dev/dialyxir_*"];
        let roots = identity(&paths, source.path()).roots;
        assert_eq!(roots[0].path, source.path().join("src"));
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&paths, target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        let web = target.path().join("src/web/_build/dev");
        assert_eq!(
            fs::read(web.join("dialyxir_erlang.plt/plt")).unwrap(),
            b"web"
        );
        assert!(target.path().join("src/api/_build/dev").exists());
        assert!(!web.join("other").exists());
    }

    #[test]
    fn a_star_does_not_cross_a_directory_and_a_leading_glob_roots_at_the_checkout() {
        assert_eq!(split_glob("a/b/c"), ("a/b/c", None));
        assert_eq!(split_glob("a/b*/c"), ("a", Some("b*/c")));
        assert_eq!(split_glob("*.txt"), ("", Some("*.txt")));
        assert_eq!(split_glob("/opt*/x"), ("/", Some("opt*/x")));
        assert_eq!(split_glob("app/[id]/{a,b}"), ("app/[id]/{a,b}", None));
        let roots = identity(&["a/*/c"], Path::new("/w")).roots;
        assert!(roots[0].selects(Path::new("b/c")));
        assert!(roots[0].selects(Path::new("b/c/inside")));
        assert!(!roots[0].selects(Path::new("b/d/c")));
        assert!(!roots[0].selects(Path::new("")));
    }

    #[test]
    fn a_glob_matching_nothing_stores_nothing_and_an_unreadable_neighbour_is_skipped() {
        let state = Arc::new(Mutex::new(Remote::default()));
        let remote = backend(&serve(Arc::clone(&state)));
        let source = tempfile::tempdir().unwrap();
        let home = source.path().join("home");
        fs::create_dir_all(home.join("locked")).unwrap();
        let writer = identity(&["home/*.rlib"], source.path());
        let key = writer.key("k").unwrap();
        let excludes = exclude_set(&[]).unwrap();
        assert_eq!(
            save(&writer, &key, &excludes, &remote).unwrap(),
            Saved::NothingToStore
        );

        let writer = identity(&["home/**/*.rlib"], source.path());
        let key = writer.key("k").unwrap();
        fs::write(home.join("a.rlib"), b"lib").unwrap();
        fs::set_permissions(home.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let saved = save(&writer, &key, &excludes, &remote);
        fs::set_permissions(home.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(saved.unwrap(), Saved::Stored { chunks: 1 });
    }

    #[test]
    fn a_shallow_glob_does_not_enter_directories_it_cannot_match() {
        let roots = identity(&["home/*.x"], Path::new("/w")).roots;
        assert!(roots[0].reaches(Path::new("a.x")));
        assert!(!roots[0].reaches(Path::new("cache/deep")));
        let roots = identity(&["home/*/_build"], Path::new("/w")).roots;
        assert!(roots[0].reaches(Path::new("app/_build")));
        assert!(roots[0].reaches(Path::new("app/_build/dev/file")));
        assert!(!roots[0].reaches(Path::new("app/src/file")));
    }

    #[test]
    fn a_dangling_root_is_nothing_to_store() {
        let state = Arc::new(Mutex::new(Remote::default()));
        let remote = backend(&serve(Arc::clone(&state)));
        let source = tempfile::tempdir().unwrap();
        symlink(source.path().join("gone"), source.path().join("alias")).unwrap();
        let writer = identity(&["alias"], source.path());
        let key = writer.key("k").unwrap();
        let saved = save(&writer, &key, &exclude_set(&[]).unwrap(), &remote).unwrap();
        assert_eq!(saved, Saved::NothingToStore);
        assert_eq!(lock(&state).commits, 0);
    }

    #[test]
    fn a_read_only_root_can_be_restored_over_again() {
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("file"), b"x").unwrap();
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o555)).unwrap();
        let roots = identity(&["tree"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o755)).unwrap();

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        extract(&bytes[..], &roots).unwrap();
        let restored = target.path().join("tree");
        assert_eq!(fs::metadata(&restored).unwrap().mode() & 0o777, 0o555);
        fs::set_permissions(&restored, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `/tmp` is writable but belongs to root, like a declared `~` that is
    /// not ours to chmod. Its stored mode is its real one, in case this test
    /// runs as root.
    #[test]
    fn a_root_that_cannot_be_chmodded_still_restores_its_files() {
        let name = format!("aster-dir-cache-test-{}", std::process::id());
        let bytes = compressed_tar(|builder| {
            let mut root = tempfile_header(tar::EntryType::Directory, 0o1777);
            builder.append_data(&mut root, "0", io::empty()).unwrap();
            let mut file = tempfile_header(tar::EntryType::Regular, 0o644);
            file.set_size(1);
            builder
                .append_data(&mut file, format!("0/{name}"), &b"x"[..])
                .unwrap();
        });
        let roots = identity(&["/tmp"], Path::new("/")).roots;
        let result = extract(&bytes[..], &roots);
        let restored = fs::read(Path::new("/tmp").join(&name));
        let _ = fs::remove_file(Path::new("/tmp").join(&name));
        result.unwrap();
        assert_eq!(restored.unwrap(), b"x");
    }

    fn tempfile_header(kind: tar::EntryType, mode: u32) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(0);
        header.set_mode(mode);
        header.set_mtime(1_600_000_000);
        header
    }

    #[test]
    fn a_file_replaces_an_existing_symlink_instead_of_writing_through_it() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("tree")).unwrap();
        fs::write(source.path().join("tree/file"), b"new").unwrap();
        let roots = identity(&["tree"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        let outside = target.path().join("outside");
        fs::write(&outside, b"untouched").unwrap();
        fs::create_dir(target.path().join("tree")).unwrap();
        symlink(&outside, target.path().join("tree/file")).unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        assert_eq!(fs::read(target.path().join("tree/file")).unwrap(), b"new");
        assert!(!fs::symlink_metadata(target.path().join("tree/file"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn long_names_and_long_link_targets_round_trip() {
        let source = tempfile::tempdir().unwrap();
        let deep = "segment-é-0123456789/".repeat(12);
        let tree = source.path().join("tree");
        fs::create_dir_all(tree.join(&deep)).unwrap();
        let name = format!("{deep}file");
        fs::write(tree.join(&name), b"deep").unwrap();
        symlink(&name, tree.join("link")).unwrap();
        fs::hard_link(tree.join(&name), tree.join("also")).unwrap();
        let roots = identity(&["tree"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();

        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        extract(&bytes[..], &roots).unwrap();
        let tree = target.path().join("tree");
        assert_eq!(fs::read(tree.join(&name)).unwrap(), b"deep");
        assert_eq!(fs::read(tree.join("also")).unwrap(), b"deep");
        assert_eq!(fs::read_link(tree.join("link")).unwrap(), Path::new(&name));
    }

    #[test]
    fn a_truncated_or_corrupt_stream_is_an_error() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("tree")).unwrap();
        fs::write(source.path().join("tree/file"), vec![9_u8; 100_000]).unwrap();
        let roots = identity(&["tree"], source.path()).roots;
        let bytes = pack(&roots, &exclude_set(&[]).unwrap(), Vec::new()).unwrap();
        let target = tempfile::tempdir().unwrap();
        let roots = identity(&["tree"], target.path()).roots;
        assert!(extract(&bytes[..bytes.len() / 2], &roots).is_err());
        assert!(extract(&b"not zstd"[..], &roots).is_err());
    }
}
