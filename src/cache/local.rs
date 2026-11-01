//! Local content-addressed blob store and action-cache manifests.
//!
//! Blobs live at `<cache_dir>/cas/<aa>/<digest>.zst`, compressed with zstd and
//! keyed by the sha256 of the uncompressed bytes. Manifests live at
//! `<cache_dir>/ac/<aa>/<key>.json`. Writes go to a same-directory temp file
//! and rename into place.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use sha2::{Digest as Sha256Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::artifact::{Digest, Manifest};
use super::hasher::encode_hex;

const SHARD_LEN: usize = 2;

/// How many times `put_blob` restarts when a concurrent prune unlinks the
/// destination between the exists check and the rename.
const PUT_BLOB_ATTEMPTS: u32 = 2;

/// Prune keeps a blob that no manifest names when it is younger than this.
/// A writer stores blobs before their manifest; this window covers that gap.
const UNREFERENCED_BLOB_GRACE: Duration = Duration::from_secs(60 * 60);

/// Local CAS and action-cache tier under a resolved cache directory.
pub struct LocalStore {
    cache_dir: PathBuf,
}

impl LocalStore {
    /// `cache_dir` is the directory that holds `cas/` and `ac/`.
    /// Callers resolve it before constructing the store.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Compress `reader` and store it under `digest`.
    ///
    /// The digest must be the sha256 of the uncompressed bytes. An existing
    /// blob with the same digest is left in place. A missing destination
    /// after a failed rename is retried: a concurrent prune may have unlinked
    /// the blob between the exists check and the rename.
    pub fn put_blob(&self, digest: &Digest, mut reader: impl Read) -> Result<()> {
        let mut buffered = Vec::new();
        reader
            .read_to_end(&mut buffered)
            .context("Failed to read blob")?;
        // Shared with other writers, exclusive against prune: a sweep never
        // runs while this blob is between temp file and rename.
        let _lock = self.acquire_lock(LockMode::Shared)?;
        let mut last_error = None;
        for _ in 0..PUT_BLOB_ATTEMPTS {
            match self.put_blob_once(digest, buffered.as_slice()) {
                Ok(()) => return Ok(()),
                Err(error) if missing_after_race(&error, digest, self) => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("blob put failed")))
    }

    /// [`Self::put_blob`] reading from a file instead of memory.
    ///
    /// Each attempt seeks `file` back to its start, so a large blob is never
    /// held in memory.
    pub(crate) fn put_blob_file(&self, digest: &Digest, file: &mut File) -> Result<()> {
        let _lock = self.acquire_lock(LockMode::Shared)?;
        let mut last_error = None;
        for _ in 0..PUT_BLOB_ATTEMPTS {
            file.seek(SeekFrom::Start(0))
                .context("Failed to rewind blob spool")?;
            match self.put_blob_once(digest, &mut *file) {
                Ok(()) => return Ok(()),
                Err(error) if missing_after_race(&error, digest, self) => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("blob put failed")))
    }

    /// Unnamed temp file under the cache directory, deleted when dropped.
    /// Holds a blob in transit between the local store and a remote.
    pub(crate) fn spool(&self) -> Result<File> {
        fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("Failed to create {}", self.cache_dir.display()))?;
        tempfile::tempfile_in(&self.cache_dir).with_context(|| {
            format!(
                "Failed to create a spool file in {}",
                self.cache_dir.display()
            )
        })
    }

    fn put_blob_once(&self, digest: &Digest, bytes: impl Read) -> Result<()> {
        let path = self.blob_path(digest)?;
        if path.is_file() {
            // Refresh mtime so prune's grace window covers this writer until
            // it stores the manifest that names the blob.
            touch_mtime(&path)?;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        let tmp = temp_path(&path);
        let write_result = (|| {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)
                .with_context(|| format!("Failed to open {}", tmp.display()))?;
            let (written, encoder) = hash_and_compress(bytes, file)?;
            encoder.finish().context("Failed to finish zstd encoder")?;
            if written.as_str() != digest.as_str() {
                bail!(
                    "blob digest mismatch: expected {}, hashed {}",
                    digest.as_str(),
                    written.as_str()
                );
            }
            fs::rename(&tmp, &path).with_context(|| {
                format!("Failed to rename {} to {}", tmp.display(), path.display())
            })
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        // A concurrent put may have renamed its own temp into place first.
        // Only then is a lost race success: our bytes were not stored, but
        // the digest that won is the same content.
        if write_result.is_err() && path.is_file() {
            let _ = fs::remove_file(&tmp);
            return Ok(());
        }
        write_result
    }

    /// Decompress `digest` into `writer`.
    ///
    /// The uncompressed bytes are hashed. A mismatch is an error and the blob
    /// is deleted so a later read does not restore the wrong content.
    pub fn get_blob(&self, digest: &Digest, writer: impl Write) -> Result<()> {
        let path = self.blob_path(digest)?;
        let file =
            File::open(&path).with_context(|| format!("Failed to open blob {}", path.display()))?;
        let outcome = read_blob(&file, digest, writer);
        match outcome {
            Ok(()) => {
                touch_mtime(&path)?;
                Ok(())
            }
            Err(error) => {
                // Delete only a corrupt frame or a digest mismatch, and only
                // while the path still names the inode this read opened. A
                // caller write failure leaves the stored blob in place.
                if error.downcast_ref::<CorruptBlob>().is_some() && same_file(&file, &path) {
                    let _ = fs::remove_file(&path);
                }
                Err(error)
            }
        }
    }

    /// Whether a blob file exists for `digest`. Does not verify contents.
    pub fn has_blob(&self, digest: &Digest) -> bool {
        self.blob_path(digest)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    /// Write `manifest` only when every referenced blob is present.
    ///
    /// The blob check and the write run under a shared `cache.lock`, so a
    /// prune cannot delete a checked blob before the manifest lands.
    pub fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
        let _lock = self.acquire_lock(LockMode::Shared)?;
        for digest in manifest.blob_refs() {
            if !self.has_blob(&digest) {
                bail!("manifest references missing blob {}", digest.as_str());
            }
        }
        let path = self.manifest_path(&manifest.key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let bytes = manifest.encode();
        write_atomic(&path, &bytes)
    }

    /// Drop old and least-recently-used manifests, then delete blobs no
    /// surviving manifest references. Also delete `*.tmp.*` files under
    /// `cas/` and `ac/` older than `UNREFERENCED_BLOB_GRACE`.
    ///
    /// Manifest mtime is the access time (`get_manifest` touches it). Age is
    /// applied first. Size is the sum of on-disk `cas/**/*.zst` lengths plus
    /// leftover temp files; oldest manifests are removed until that sum is at
    /// or under `max_size`. A temp file younger than the grace counts toward
    /// that sum and is never deleted. Stale temp files are removed before the
    /// size check so they do not evict a manifest that would otherwise fit.
    /// `None` disables that limit. An exclusive `cache.lock` is held for the
    /// whole scan, manifest delete, and blob sweep so a blob is unlinked only
    /// after every surviving manifest has been decoded. Writers hold the same
    /// lock shared. An unreferenced blob is kept while its mtime is younger
    /// than `UNREFERENCED_BLOB_GRACE` (one hour), even when only an evicted
    /// manifest named it: a writer may have just stored or refreshed it for
    /// a manifest it has not written yet. A manifest that does not decode is
    /// a cache miss; it is removed and protects no blobs.
    pub fn prune(&self, max_size: Option<u64>, max_age: Option<Duration>) -> Result<PruneStats> {
        let _lock = self.acquire_lock(LockMode::Exclusive)?;
        self.prune_locked(max_size, max_age)
    }

    fn prune_locked(&self, max_size: Option<u64>, max_age: Option<Duration>) -> Result<PruneStats> {
        let now = SystemTime::now();
        let grace_cutoff = now.checked_sub(UNREFERENCED_BLOB_GRACE);
        // A killed writer leaves `*.tmp.*` next to the blob or manifest it
        // was renaming into place. Sweep those before size accounting so a
        // stale temp does not evict a manifest the cap would otherwise keep.
        // A file newer than the grace is a live or just-killed writer: keep
        // it, but count its bytes.
        let tmp_files_removed = self.sweep_stale_tmp_files(grace_cutoff)?;
        let mut manifests = self.scan_manifests()?;
        manifests.sort_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });

        let mut referenced = HashSet::new();
        let mut survivors: Vec<&ManifestFile> = Vec::new();
        for entry in &mut manifests {
            let bytes = match fs::read(&entry.path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to read {}", entry.path.display()))
                }
            };
            // An undecodable manifest is not a survivor: it cannot protect
            // blobs, and leaving it would keep a miss that still occupies a key.
            let Some(manifest) = Manifest::decode(&bytes) else {
                continue;
            };
            entry.refs = manifest.blob_refs();
            let mut keep = true;
            if let Some(max_age) = max_age {
                if age_of(entry.modified, now) > max_age {
                    keep = false;
                }
            }
            if keep {
                referenced.extend(entry.refs.iter().cloned());
                survivors.push(entry);
            }
        }

        if let Some(max_size) = max_size {
            // Survivors are oldest-first. Drop from the front until the
            // on-disk bytes that would remain after the sweep fit. Orphans
            // are not in that total: the sweep deletes them. Fresh temp files
            // are: they are never deleted. A blob shared with a newer
            // manifest stays referenced, so it is never deleted.
            let blob_sizes = self.blob_sizes()?;
            let tmp_bytes = self.tmp_bytes()?;
            let mut index = 0;
            while index < survivors.len()
                && remaining_bytes(&blob_sizes, &referenced).saturating_add(tmp_bytes) > max_size
            {
                index += 1;
                referenced.clear();
                for entry in &survivors[index..] {
                    referenced.extend(entry.refs.iter().cloned());
                }
            }
            survivors = survivors[index..].to_vec();
        }

        let survivor_paths: HashSet<PathBuf> =
            survivors.iter().map(|entry| entry.path.clone()).collect();
        drop(survivors);
        let mut manifests_removed = 0u64;
        for entry in &manifests {
            if survivor_paths.contains(&entry.path) {
                continue;
            }
            match fs::remove_file(&entry.path) {
                Ok(()) => manifests_removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to remove manifest {}", entry.path.display())
                    })
                }
            }
        }

        let blobs_removed = self.sweep_unreferenced_blobs(&referenced, grace_cutoff)?;
        Ok(PruneStats {
            manifests_removed,
            blobs_removed,
            tmp_files_removed,
            bytes_remaining: self.cas_bytes()?,
        })
    }

    /// On-disk size of every `cas/**/*.zst` blob plus leftover `*.tmp.*`
    /// files under `cas/` and `ac/`.
    pub fn cas_bytes(&self) -> Result<u64> {
        let blobs = self.blob_sizes()?.into_values().sum::<u64>();
        Ok(blobs.saturating_add(self.tmp_bytes()?))
    }

    fn blob_sizes(&self) -> Result<HashMap<Digest, u64>> {
        let mut sizes = HashMap::new();
        for path in self.blob_files()? {
            let Some(digest) = digest_from_blob_path(&path) else {
                continue;
            };
            let len = fs::metadata(&path)
                .with_context(|| format!("Failed to stat {}", path.display()))?
                .len();
            sizes.insert(digest, len);
        }
        Ok(sizes)
    }

    fn sweep_unreferenced_blobs(
        &self,
        referenced: &HashSet<Digest>,
        grace_cutoff: Option<SystemTime>,
    ) -> Result<u64> {
        let mut removed = 0u64;
        for path in self.blob_files()? {
            let Some(digest) = digest_from_blob_path(&path) else {
                continue;
            };
            if referenced.contains(&digest) {
                continue;
            }
            // Grace covers blobs an evicted manifest named too: put_blob
            // refreshes mtime on reuse, so a writer's fresh blob survives.
            if written_after(&path, grace_cutoff) {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to remove blob {}", path.display()))
                }
            }
        }
        Ok(removed)
    }

    fn scan_manifests(&self) -> Result<Vec<ManifestFile>> {
        let root = self.cache_dir.join("ac");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        for shard in read_dir_files(&root)? {
            if !shard.path.is_dir() {
                continue;
            }
            for entry in read_dir_files(&shard.path)? {
                if !entry.path.is_file() {
                    continue;
                }
                let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !name.ends_with(".json") || name.contains(".tmp.") {
                    continue;
                }
                let modified = entry
                    .modified
                    .with_context(|| format!("Failed to read mtime of {}", entry.path.display()))?;
                found.push(ManifestFile {
                    path: entry.path,
                    modified,
                    refs: Vec::new(),
                });
            }
        }
        Ok(found)
    }

    fn blob_files(&self) -> Result<Vec<PathBuf>> {
        let root = self.cache_dir.join("cas");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        for shard in read_dir_files(&root)? {
            if !shard.path.is_dir() {
                continue;
            }
            for entry in read_dir_files(&shard.path)? {
                if !entry.path.is_file() {
                    continue;
                }
                let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.ends_with(".zst") && !name.contains(".tmp.") {
                    found.push(entry.path);
                }
            }
        }
        Ok(found)
    }

    /// `*.tmp.*` files under `cas/*/` and `ac/*/`, the names [`temp_path`]
    /// writes before an atomic rename.
    fn tmp_files(&self) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for root_name in ["cas", "ac"] {
            let root = self.cache_dir.join(root_name);
            if !root.exists() {
                continue;
            }
            for shard in read_dir_files(&root)? {
                if !shard.path.is_dir() {
                    continue;
                }
                for entry in read_dir_files(&shard.path)? {
                    if !entry.path.is_file() {
                        continue;
                    }
                    let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    if name.contains(".tmp.") {
                        found.push(entry.path);
                    }
                }
            }
        }
        Ok(found)
    }

    fn tmp_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        for path in self.tmp_files()? {
            let len = match fs::metadata(&path) {
                Ok(meta) => meta.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("Failed to stat {}", path.display()))
                }
            };
            total = total.saturating_add(len);
        }
        Ok(total)
    }

    fn sweep_stale_tmp_files(&self, grace_cutoff: Option<SystemTime>) -> Result<u64> {
        let mut removed = 0u64;
        for path in self.tmp_files()? {
            // A live writer keeps updating mtime. Never delete a temp newer
            // than the grace, even if its process has already exited.
            if written_after(&path, grace_cutoff) {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to remove temp file {}", path.display()))
                }
            }
        }
        Ok(removed)
    }

    fn acquire_lock(&self, mode: LockMode) -> Result<File> {
        let lock_path = self.cache_dir.join("cache.lock");
        fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("Failed to create {}", self.cache_dir.display()))?;
        // truncate(false): the lock lives on this inode. Replacing it would
        // leave holders of the old inode unlocked against new openers.
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;
        // UFCS: std::fs::File locking is 1.89; this crate's MSRV is 1.88.
        match mode {
            LockMode::Shared => FileExt::lock_shared(&lock_file)
                .with_context(|| "Failed to acquire shared cache lock")?,
            LockMode::Exclusive => FileExt::lock_exclusive(&lock_file)
                .with_context(|| "Failed to acquire cache lock")?,
        }
        Ok(lock_file)
    }

    /// Load the manifest for `key`.
    ///
    /// `None` is a cache miss: the file is absent, or [`Manifest::decode`]
    /// rejects it. A hit touches the file mtime.
    pub fn get_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        let path = self.manifest_path(key)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()))
            }
        };
        match Manifest::decode(&bytes) {
            Some(manifest) => {
                touch_mtime(&path)?;
                Ok(Some(manifest))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn blob_path(&self, digest: &Digest) -> Result<PathBuf> {
        let hex = digest.as_str();
        let shard = shard(hex).context("blob digest is not a hex sha256")?;
        Ok(self
            .cache_dir
            .join("cas")
            .join(shard)
            .join(format!("{hex}.zst")))
    }

    pub(crate) fn manifest_path(&self, key: &str) -> Result<PathBuf> {
        let shard = shard(key).context("manifest key is not a hex cache hash")?;
        Ok(self
            .cache_dir
            .join("ac")
            .join(shard)
            .join(format!("{key}.json")))
    }
}

/// Counts from one prune.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PruneStats {
    /// Manifest files removed, including undecodable ones.
    pub manifests_removed: u64,
    /// Blob files removed because no surviving manifest referenced them.
    pub blobs_removed: u64,
    /// `*.tmp.*` files removed because they were older than the grace period.
    pub tmp_files_removed: u64,
    /// On-disk size of remaining blobs plus remaining `*.tmp.*` files.
    pub bytes_remaining: u64,
}

struct ManifestFile {
    path: PathBuf,
    modified: SystemTime,
    /// Blobs named by a decoded manifest. Empty when the file did not decode.
    refs: Vec<Digest>,
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

/// Whether `path` was modified after `cutoff`. An unreadable mtime counts as
/// recent so the blob is kept.
fn written_after(path: &Path, cutoff: Option<SystemTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return true;
    };
    match fs::metadata(path).and_then(|meta| meta.modified()) {
        Ok(modified) => modified > cutoff,
        Err(_) => true,
    }
}

struct DirEntry {
    path: PathBuf,
    modified: io::Result<SystemTime>,
}

fn read_dir_files(dir: &Path) -> Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    let read = fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))?;
    for entry in read {
        let entry = entry.with_context(|| format!("Failed to read {}", dir.display()))?;
        entries.push(DirEntry {
            modified: entry.metadata().and_then(|meta| meta.modified()),
            path: entry.path(),
        });
    }
    Ok(entries)
}

fn age_of(modified: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(modified).unwrap_or(Duration::ZERO)
}

/// Bytes that remain after the sweep: every on-disk blob a survivor names.
/// Orphans are not included; the sweep deletes them.
fn remaining_bytes(blob_sizes: &HashMap<Digest, u64>, referenced: &HashSet<Digest>) -> u64 {
    referenced
        .iter()
        .filter_map(|digest| blob_sizes.get(digest))
        .sum()
}

fn digest_from_blob_path(path: &Path) -> Option<Digest> {
    let name = path.file_name()?.to_str()?;
    let hex = name.strip_suffix(".zst")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Digest::from_hex(hex.to_ascii_lowercase()))
}

/// True when a put lost the race to a prune that unlinked the destination.
fn missing_after_race(error: &anyhow::Error, digest: &Digest, store: &LocalStore) -> bool {
    let message = format!("{error:#}");
    message.contains("Failed to rename") && !store.has_blob(digest)
}

/// First two hex characters, after rejecting anything that is not a safe key.
fn shard(key: &str) -> Option<&str> {
    if key.len() < SHARD_LEN || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(&key[..SHARD_LEN])
}

/// The stored frame is not the bytes `digest` names.
#[derive(Debug)]
struct CorruptBlob;

impl std::fmt::Display for CorruptBlob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("corrupt blob")
    }
}

impl std::error::Error for CorruptBlob {}

fn read_blob(file: &File, digest: &Digest, writer: impl Write) -> Result<()> {
    let decoder = zstd::stream::read::Decoder::new(file).map_err(|error| {
        anyhow::Error::new(CorruptBlob)
            .context(error)
            .context("Failed to open zstd decoder")
    })?;
    let (written, _copied) = hash_and_copy(decoder, writer)?;
    if written.as_str() != digest.as_str() {
        return Err(anyhow::Error::new(CorruptBlob).context(format!(
            "blob digest mismatch: expected {}, hashed {}",
            digest.as_str(),
            written.as_str()
        )));
    }
    Ok(())
}

fn same_file(file: &File, path: &Path) -> bool {
    let Ok(open_meta) = file.metadata() else {
        return false;
    };
    let Ok(path_meta) = fs::metadata(path) else {
        return false;
    };
    open_meta_same(&open_meta, &path_meta)
}

#[cfg(unix)]
fn open_meta_same(open_meta: &fs::Metadata, path_meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    open_meta.dev() == path_meta.dev() && open_meta.ino() == path_meta.ino()
}

#[cfg(not(unix))]
fn open_meta_same(open_meta: &fs::Metadata, path_meta: &fs::Metadata) -> bool {
    // Without an inode, only delete when nothing else has replaced the path
    // since this read opened it. A replaced file has a newer mtime.
    open_meta.modified().ok() == path_meta.modified().ok() && open_meta.len() == path_meta.len()
}

fn temp_path(final_path: &Path) -> PathBuf {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("blob");
    final_path.with_file_name(format!("{name}.tmp.{}.{seq}", std::process::id()))
}

/// Hash uncompressed bytes while writing a zstd frame.
fn hash_and_compress(
    mut reader: impl Read,
    file: File,
) -> Result<(Digest, zstd::stream::write::Encoder<'static, File>)> {
    let mut encoder = zstd::stream::write::Encoder::new(file, zstd::DEFAULT_COMPRESSION_LEVEL)
        .context("Failed to open zstd encoder")?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).context("Failed to read blob")?;
        if n == 0 {
            break;
        }
        Sha256Digest::update(&mut hasher, &buf[..n]);
        encoder
            .write_all(&buf[..n])
            .context("Failed to compress blob")?;
    }
    Ok((digest_from_hasher(hasher), encoder))
}

/// Lowercase hex sha256. `Digest` keeps its string private, so the store
/// builds one the same way `Digest::of_reader` does.
fn digest_from_hasher(hasher: Sha256) -> Digest {
    Digest::from_hex(encode_hex(Sha256Digest::finalize(hasher).as_ref()))
}

fn hash_and_copy(mut reader: impl Read, mut writer: impl Write) -> Result<(Digest, u64)> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|error| {
            anyhow::Error::new(CorruptBlob)
                .context(error)
                .context("Failed to read blob")
        })?;
        if n == 0 {
            break;
        }
        Sha256Digest::update(&mut hasher, &buf[..n]);
        writer
            .write_all(&buf[..n])
            .context("Failed to write blob")?;
        copied += n as u64;
    }
    Ok((digest_from_hasher(hasher), copied))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(path);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("Failed to open {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn touch_mtime(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} to record access", path.display()))?;
    file.set_modified(SystemTime::now())
        .with_context(|| format!("Failed to record access time for {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn sample_manifest(key: &str, blobs: &[Digest]) -> Manifest {
        use super::super::artifact::{OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION};
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: key.into(),
            target: "//apps/web:build".into(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-13T00:00:00Z".into(),
            outputs: blobs
                .iter()
                .enumerate()
                .map(|(index, digest)| OutputEntry {
                    path: format!("dist/{index}.bin"),
                    kind: OutputKind::File,
                    digest: Some(digest.clone()),
                    mode: 0o644,
                    target: None,
                })
                .collect(),
            log: None,
            aster_version: "0.17.2".into(),
            packs: Vec::new(),
        }
    }

    #[test]
    fn put_get_blob_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let bytes = b"content-addressed artifact";
        let digest = Digest::of_bytes(bytes);

        store.put_blob(&digest, &bytes[..]).unwrap();
        assert!(store.has_blob(&digest));

        let path = store.blob_path(&digest).unwrap();
        assert!(path.ends_with(format!(
            "cas/{}/{}.zst",
            &digest.as_str()[..2],
            digest.as_str()
        )));
        let stored = fs::read(&path).unwrap();
        assert_ne!(stored, bytes);
        assert_eq!(zstd::decode_all(&stored[..]).unwrap(), bytes);

        let mut out = Vec::new();
        store.get_blob(&digest, &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn get_blob_detects_corrupted_blob_and_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let digest = Digest::of_bytes(b"intact");
        store.put_blob(&digest, &b"intact"[..]).unwrap();

        let path = store.blob_path(&digest).unwrap();
        fs::write(&path, b"not a zstd frame").unwrap();

        let err = store.get_blob(&digest, &mut Vec::new()).unwrap_err();
        assert!(
            err.to_string().contains("zstd") || err.to_string().contains("blob"),
            "unexpected error: {err}"
        );
        assert!(!path.exists(), "corrupt blob should be deleted: {err}");
        assert!(!store.has_blob(&digest));
    }

    #[test]
    fn get_blob_deletes_digest_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let digest = Digest::of_bytes(b"claimed");
        // A valid zstd frame of different bytes still fails the digest check.
        let other = zstd::encode_all(&b"other"[..], 0).unwrap();
        let path = store.blob_path(&digest).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, other).unwrap();

        let err = store.get_blob(&digest, &mut Vec::new()).unwrap_err();
        assert!(
            err.to_string().contains("digest mismatch"),
            "unexpected error: {err}"
        );
        assert!(!path.exists(), "mismatched blob should be deleted");
    }

    #[test]
    fn read_touches_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let bytes = b"touched";
        let digest = Digest::of_bytes(bytes);
        store.put_blob(&digest, &bytes[..]).unwrap();
        let key = "cd0123456789abcdef";
        let manifest = sample_manifest(key, std::slice::from_ref(&digest));
        store.put_manifest(&manifest).unwrap();

        let blob_path = store.blob_path(&digest).unwrap();
        let manifest_path = store.manifest_path(key).unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        filetime_set(&blob_path, old);
        filetime_set(&manifest_path, old);

        store.get_blob(&digest, &mut Vec::new()).unwrap();
        store.get_manifest(key).unwrap().expect("hit");

        let blob_mtime = fs::metadata(&blob_path).unwrap().modified().unwrap();
        let manifest_mtime = fs::metadata(&manifest_path).unwrap().modified().unwrap();
        assert!(blob_mtime > old, "blob mtime was not touched");
        assert!(manifest_mtime > old, "manifest mtime was not touched");
    }

    struct FailWrite;

    impl Write for FailWrite {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "caller write failed",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn get_blob_keeps_blob_when_writer_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let bytes = b"keep-me";
        let digest = Digest::of_bytes(bytes);
        store.put_blob(&digest, &bytes[..]).unwrap();
        let path = store.blob_path(&digest).unwrap();

        let err = store.get_blob(&digest, FailWrite).unwrap_err();
        assert!(
            err.to_string().contains("Failed to write blob"),
            "unexpected error: {err}"
        );
        assert!(path.is_file(), "writer failure must not delete the blob");
        let mut out = Vec::new();
        store.get_blob(&digest, &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn put_manifest_rejects_missing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let digest = Digest::of_bytes(b"missing");
        let key = "ab0123456789abcdef";
        let manifest = sample_manifest(key, std::slice::from_ref(&digest));

        let err = store.put_manifest(&manifest).unwrap_err();
        assert!(
            err.to_string().contains("missing blob"),
            "unexpected error: {err}"
        );
        assert!(store.get_manifest(key).unwrap().is_none());

        store.put_blob(&digest, &b"missing"[..]).unwrap();
        store.put_manifest(&manifest).unwrap();
        let loaded = store.get_manifest(key).unwrap().expect("manifest hit");
        assert_eq!(loaded, manifest);
        let path = store.manifest_path(key).unwrap();
        assert!(path.ends_with(format!("ac/{}/{key}.json", &key[..2])));
    }

    #[test]
    fn parallel_puts_of_same_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::new(dir.path().to_path_buf()));
        let bytes = b"same-bytes-from-many-writers";
        let digest = Digest::of_bytes(bytes);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            let digest = digest.clone();
            handles.push(thread::spawn(move || {
                store.put_blob(&digest, &bytes[..]).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let mut out = Vec::new();
        store.get_blob(&digest, &mut out).unwrap();
        assert_eq!(out, bytes);
    }

    fn filetime_set(path: &Path, time: SystemTime) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    #[test]
    fn prune_keeps_newest_manifests_and_referenced_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let old_blob = Digest::of_bytes(b"old-artifact-bytes");
        let new_blob = Digest::of_bytes(b"new-artifact-bytes");
        let shared = Digest::of_bytes(b"shared-by-both");
        store
            .put_blob(&old_blob, &b"old-artifact-bytes"[..])
            .unwrap();
        store
            .put_blob(&new_blob, &b"new-artifact-bytes"[..])
            .unwrap();
        store.put_blob(&shared, &b"shared-by-both"[..]).unwrap();

        let old_key = "aa0123456789abcdef";
        let new_key = "bb0123456789abcdef";
        store
            .put_manifest(&sample_manifest(
                old_key,
                &[old_blob.clone(), shared.clone()],
            ))
            .unwrap();
        store
            .put_manifest(&sample_manifest(
                new_key,
                &[new_blob.clone(), shared.clone()],
            ))
            .unwrap();

        let old_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let new_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5_000);
        filetime_set(&store.manifest_path(old_key).unwrap(), old_time);
        filetime_set(&store.manifest_path(new_key).unwrap(), new_time);
        // Past the grace window, so eviction of its manifest frees it.
        filetime_set(&store.blob_path(&old_blob).unwrap(), old_time);

        let before = store.cas_bytes().unwrap();
        let new_only = fs::metadata(store.blob_path(&new_blob).unwrap())
            .unwrap()
            .len()
            + fs::metadata(store.blob_path(&shared).unwrap())
                .unwrap()
                .len();
        assert!(before > new_only, "fixture must exceed the survivor size");

        let stats = store.prune(Some(new_only), None).unwrap();
        assert_eq!(stats.manifests_removed, 1);
        assert!(store.get_manifest(old_key).unwrap().is_none());
        assert_eq!(
            store
                .get_manifest(new_key)
                .unwrap()
                .expect("newest survives")
                .key,
            new_key
        );
        assert!(
            !store.has_blob(&old_blob),
            "unreferenced blob must be deleted"
        );
        assert!(store.has_blob(&new_blob));
        assert!(
            store.has_blob(&shared),
            "blob referenced by a surviving manifest must remain"
        );
        assert!(stats.bytes_remaining <= new_only);
        assert_eq!(stats.blobs_removed, 1);
    }

    #[test]
    fn prune_sweeps_orphans_without_dropping_fitting_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let orphan_bytes: Vec<u8> = (0..4096).map(|index| (index * 17) as u8).collect();
        let orphan = Digest::of_bytes(&orphan_bytes);
        let old_blob = Digest::of_bytes(b"old-still-referenced");
        let new_blob = Digest::of_bytes(b"new-still-referenced");
        store.put_blob(&orphan, orphan_bytes.as_slice()).unwrap();
        filetime_set(
            &store.blob_path(&orphan).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        store
            .put_blob(&old_blob, &b"old-still-referenced"[..])
            .unwrap();
        store
            .put_blob(&new_blob, &b"new-still-referenced"[..])
            .unwrap();

        let old_key = "cc0123456789abcdef";
        let new_key = "dd0123456789abcdef";
        store
            .put_manifest(&sample_manifest(old_key, std::slice::from_ref(&old_blob)))
            .unwrap();
        store
            .put_manifest(&sample_manifest(new_key, std::slice::from_ref(&new_blob)))
            .unwrap();
        filetime_set(
            &store.manifest_path(old_key).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        filetime_set(
            &store.manifest_path(new_key).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5_000),
        );

        // The orphan is unreferenced, so a sweep alone would bring the store
        // under this cap. Prune must not also drop a manifest.
        let orphan_len = fs::metadata(store.blob_path(&orphan).unwrap())
            .unwrap()
            .len();
        let referenced = store.cas_bytes().unwrap() - orphan_len;
        assert!(orphan_len > referenced);
        let stats = store.prune(Some(referenced), None).unwrap();
        assert_eq!(stats.manifests_removed, 0);
        assert!(store.get_manifest(old_key).unwrap().is_some());
        assert!(store.get_manifest(new_key).unwrap().is_some());
        assert!(!store.has_blob(&orphan));
        assert!(store.has_blob(&old_blob));
        assert!(store.has_blob(&new_blob));
        assert!(stats.bytes_remaining <= referenced);
    }

    #[test]
    fn prune_drops_oldest_when_survivors_still_exceed_cap_after_orphan_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let orphan_bytes: Vec<u8> = (0..2048).map(|index| (index * 17) as u8).collect();
        let old_bytes: Vec<u8> = (0..2048).map(|index| (index * 31) as u8).collect();
        let orphan = Digest::of_bytes(&orphan_bytes);
        let old_blob = Digest::of_bytes(&old_bytes);
        let new_blob = Digest::of_bytes(b"tiny-survivor");
        store.put_blob(&orphan, orphan_bytes.as_slice()).unwrap();
        filetime_set(
            &store.blob_path(&orphan).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        store.put_blob(&old_blob, old_bytes.as_slice()).unwrap();
        filetime_set(
            &store.blob_path(&old_blob).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        store.put_blob(&new_blob, &b"tiny-survivor"[..]).unwrap();

        let old_key = "ee0123456789abcdef";
        let new_key = "ff0123456789abcdef";
        store
            .put_manifest(&sample_manifest(old_key, std::slice::from_ref(&old_blob)))
            .unwrap();
        store
            .put_manifest(&sample_manifest(new_key, std::slice::from_ref(&new_blob)))
            .unwrap();
        filetime_set(
            &store.manifest_path(old_key).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        filetime_set(
            &store.manifest_path(new_key).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5_000),
        );

        let survivor = fs::metadata(store.blob_path(&new_blob).unwrap())
            .unwrap()
            .len();
        let stats = store.prune(Some(survivor), None).unwrap();
        assert!(store.get_manifest(old_key).unwrap().is_none());
        assert!(store.get_manifest(new_key).unwrap().is_some());
        assert!(!store.has_blob(&orphan));
        assert!(!store.has_blob(&old_blob));
        assert!(store.has_blob(&new_blob));
        assert!(stats.bytes_remaining <= survivor);
    }

    #[test]
    fn prune_keeps_fresh_unreferenced_blob_for_inflight_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        // A writer has stored its blob but not yet its manifest.
        let pending = Digest::of_bytes(b"pending-manifest-blob");
        store
            .put_blob(&pending, &b"pending-manifest-blob"[..])
            .unwrap();
        let stale = Digest::of_bytes(b"stale-orphan-blob");
        store.put_blob(&stale, &b"stale-orphan-blob"[..]).unwrap();
        filetime_set(
            &store.blob_path(&stale).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );

        let stats = store.prune(Some(0), None).unwrap();
        assert!(store.has_blob(&pending), "in-flight blob must survive");
        assert!(!store.has_blob(&stale), "stale orphan must be swept");
        assert_eq!(stats.blobs_removed, 1);

        // The writer can now publish the manifest that names the blob.
        let key = "ab0123456789abcdef";
        store
            .put_manifest(&sample_manifest(key, std::slice::from_ref(&pending)))
            .unwrap();
        assert!(store.get_manifest(key).unwrap().is_some());
    }

    #[test]
    fn prune_keeps_fresh_blob_of_evicted_manifest_for_reusing_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let reused = Digest::of_bytes(b"reused-by-new-writer");
        store
            .put_blob(&reused, &b"reused-by-new-writer"[..])
            .unwrap();
        let old_key = "ac0123456789abcdef";
        store
            .put_manifest(&sample_manifest(old_key, std::slice::from_ref(&reused)))
            .unwrap();
        filetime_set(
            &store.manifest_path(old_key).unwrap(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        // A writer re-puts the blob for a manifest it has not stored yet.
        store
            .put_blob(&reused, &b"reused-by-new-writer"[..])
            .unwrap();

        let stats = store
            .prune(None, Some(std::time::Duration::from_secs(60)))
            .unwrap();
        assert_eq!(stats.manifests_removed, 1);
        assert!(store.get_manifest(old_key).unwrap().is_none());
        assert!(store.has_blob(&reused), "fresh blob must survive eviction");
        let new_key = "ad0123456789abcdef";
        store
            .put_manifest(&sample_manifest(new_key, std::slice::from_ref(&reused)))
            .unwrap();
    }

    #[test]
    fn put_blob_refreshes_mtime_of_existing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let digest = Digest::of_bytes(b"reused-blob");
        store.put_blob(&digest, &b"reused-blob"[..]).unwrap();
        let path = store.blob_path(&digest).unwrap();
        filetime_set(
            &path,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        store.put_blob(&digest, &b"reused-blob"[..]).unwrap();
        store.prune(Some(0), None).unwrap();
        assert!(
            store.has_blob(&digest),
            "re-put blob is in the grace window"
        );
    }

    fn plant_tmp(dir: &Path, root: &str, shard: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(root).join(shard).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn prune_sweeps_stale_tmp_keeps_fresh_held_open_tmp_and_counts_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let pid = std::process::id();
        let fresh_cas = plant_tmp(
            dir.path(),
            "cas",
            "ab",
            &format!("{}.zst.tmp.{pid}.1", "ab".repeat(32)),
            b"live-writer",
        );
        let stale_cas = plant_tmp(
            dir.path(),
            "cas",
            "ab",
            &format!("{}.zst.tmp.1.2", "cd".repeat(32)),
            &[7u8; 64],
        );
        let fresh_ac = plant_tmp(
            dir.path(),
            "ac",
            "cd",
            &format!("cd0123456789abcd.json.tmp.{pid}.1"),
            br#"{"pending":true}"#,
        );
        let stale_ac = plant_tmp(
            dir.path(),
            "ac",
            "cd",
            "cd0123456789abcd.json.tmp.1.2",
            br#"{"stale":true}"#,
        );
        let ancient = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        filetime_set(&stale_cas, ancient);
        filetime_set(&stale_ac, ancient);
        let mut held = OpenOptions::new().append(true).open(&fresh_cas).unwrap();
        let fresh_bytes =
            fs::metadata(&fresh_cas).unwrap().len() + fs::metadata(&fresh_ac).unwrap().len();

        let stats = store
            .prune(Some(1), Some(std::time::Duration::from_secs(1)))
            .unwrap();

        assert!(fresh_cas.is_file(), "fresh cas temp must survive");
        assert!(fresh_ac.is_file(), "fresh ac temp must survive");
        assert!(!stale_cas.exists(), "stale cas temp must be swept");
        assert!(!stale_ac.exists(), "stale ac temp must be swept");
        assert_eq!(stats.tmp_files_removed, 2);
        assert_eq!(stats.blobs_removed, 0);
        assert_eq!(stats.bytes_remaining, fresh_bytes);
        assert_eq!(store.cas_bytes().unwrap(), fresh_bytes);
        held.write_all(b"!").unwrap();
        assert!(
            fs::metadata(&fresh_cas).unwrap().len() > b"live-writer".len() as u64,
            "held-open writer must still own the temp file"
        );
    }

    #[test]
    fn prune_counts_fresh_tmp_bytes_toward_max_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let blob = Digest::of_bytes(b"referenced-blob");
        store.put_blob(&blob, &b"referenced-blob"[..]).unwrap();
        let key = "ee0123456789abcdef";
        store
            .put_manifest(&sample_manifest(key, std::slice::from_ref(&blob)))
            .unwrap();
        let blob_len = fs::metadata(store.blob_path(&blob).unwrap()).unwrap().len();
        let fresh = plant_tmp(
            dir.path(),
            "cas",
            "ab",
            &format!("{}.zst.tmp.{}.1", "ab".repeat(32), std::process::id()),
            &vec![9u8; 8_192],
        );
        let tmp_len = fs::metadata(&fresh).unwrap().len();
        assert!(tmp_len > blob_len);

        let stats = store.prune(Some(blob_len), None).unwrap();
        assert_eq!(
            stats.manifests_removed, 1,
            "fresh temp bytes must force eviction"
        );
        assert!(store.get_manifest(key).unwrap().is_none());
        assert!(fresh.is_file(), "fresh temp is never deleted");
        assert_eq!(stats.tmp_files_removed, 0);
        // The blob is still inside the grace window, so the sweep keeps it.
        assert_eq!(stats.bytes_remaining, blob_len + tmp_len);
    }

    #[test]
    fn prune_does_not_evict_a_fitting_manifest_for_a_stale_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        let blob = Digest::of_bytes(b"keep-me");
        store.put_blob(&blob, &b"keep-me"[..]).unwrap();
        let key = "ef0123456789abcdef";
        store
            .put_manifest(&sample_manifest(key, std::slice::from_ref(&blob)))
            .unwrap();
        let blob_len = fs::metadata(store.blob_path(&blob).unwrap()).unwrap().len();
        let stale = plant_tmp(
            dir.path(),
            "ac",
            "ef",
            "ef0123456789abcdef.json.tmp.1.9",
            &vec![1u8; 8_192],
        );
        filetime_set(
            &stale,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
        );
        let before = store.cas_bytes().unwrap();
        assert!(
            before > blob_len,
            "stale temp must be in the size before prune"
        );

        let stats = store.prune(Some(blob_len), None).unwrap();
        assert_eq!(stats.manifests_removed, 0);
        assert!(store.get_manifest(key).unwrap().is_some());
        assert!(store.has_blob(&blob));
        assert!(!stale.exists());
        assert_eq!(stats.tmp_files_removed, 1);
        assert_eq!(stats.bytes_remaining, blob_len);
    }
}
