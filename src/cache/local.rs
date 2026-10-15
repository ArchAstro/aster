//! Local content-addressed blob store and action-cache manifests.
//!
//! Blobs live at `<cache_dir>/cas/<aa>/<digest>.zst`, compressed with zstd and
//! keyed by the sha256 of the uncompressed bytes. Manifests live at
//! `<cache_dir>/ac/<aa>/<key>.json`. Writes go to a same-directory temp file
//! and rename into place.

use anyhow::{bail, Context, Result};
use sha2::{Digest as Sha256Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::artifact::{Digest, Manifest};
use super::hasher::encode_hex;

const SHARD_LEN: usize = 2;

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
    /// blob with the same digest is left in place.
    pub fn put_blob(&self, digest: &Digest, reader: impl Read) -> Result<()> {
        let path = self.blob_path(digest)?;
        if path.is_file() {
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
            let (written, encoder) = hash_and_compress(reader, file)?;
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
    pub fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
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

    fn blob_path(&self, digest: &Digest) -> Result<PathBuf> {
        let hex = digest.as_str();
        let shard = shard(hex).context("blob digest is not a hex sha256")?;
        Ok(self
            .cache_dir
            .join("cas")
            .join(shard)
            .join(format!("{hex}.zst")))
    }

    fn manifest_path(&self, key: &str) -> Result<PathBuf> {
        let shard = shard(key).context("manifest key is not a hex cache hash")?;
        Ok(self
            .cache_dir
            .join("ac")
            .join(shard)
            .join(format!("{key}.json")))
    }
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
}
