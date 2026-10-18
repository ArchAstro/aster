//! Synchronous cache backend and the local-then-remote tier.
//!
//! [`TieredCache`] always reads and writes [`LocalStore`] first. A remote
//! [`CacheBackend`] is optional. Remote I/O never fails a build: the first
//! error is logged, and after [`FAILURES_BEFORE_DISABLE`] consecutive
//! failures the remote is skipped for the rest of the process.
//!
//! Remote clients (aster-srv, GitHub Actions, HTTP) are separate tasks.
//! This module only selects which kind would be used and, until those
//! clients exist, runs local-only.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::artifact::{Digest, Manifest};
use super::local::LocalStore;

/// Consecutive remote failures before the remote is skipped for the run.
pub const FAILURES_BEFORE_DISABLE: u32 = 3;

/// `ASTER_CACHE_MODE`. Unset is [`CacheMode::ReadWrite`].
pub const CACHE_MODE_ENV: &str = "ASTER_CACHE_MODE";

/// `ASTER_REMOTE_CACHE`. Unset is [`RemoteKind::AsterSrv`].
pub const REMOTE_CACHE_ENV: &str = "ASTER_REMOTE_CACHE";

/// Set when an aster-srv gateway should be used. The client is another task.
pub const ASTER_SRV_URL_ENV: &str = "ASTER_SRV_URL";

/// Remote participation for this process.
///
/// `off` skips the remote entirely. It does not disable the local store:
/// `--no-cache` still does that by not constructing a cache at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Do not contact the remote.
    Off,
    /// Download remote hits. Never call `put_*`.
    Read,
    /// Upload local stores. A remote hit is still downloaded so restore can
    /// read blobs from [`LocalStore`]. Uploads are the only write.
    Write,
    /// Download and upload.
    ReadWrite,
}

impl CacheMode {
    /// Parse `off|read|write|rw`. Anything else is an error.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "off" => Ok(Self::Off),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "rw" => Ok(Self::ReadWrite),
            other => anyhow::bail!(
                "invalid {CACHE_MODE_ENV}={other:?}; expected off, read, write, or rw"
            ),
        }
    }

    /// `ASTER_CACHE_MODE`, or [`CacheMode::ReadWrite`] when unset or empty.
    pub fn from_env() -> Result<Self> {
        match std::env::var(CACHE_MODE_ENV) {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            _ => Ok(Self::ReadWrite),
        }
    }

    fn reads(self) -> bool {
        matches!(self, Self::Read | Self::Write | Self::ReadWrite)
    }

    fn writes(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

/// Which remote the process would use. Clients land in later tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    None,
    AsterSrv,
    Gha,
    Http,
}

impl RemoteKind {
    /// Parse `none|aster-srv|gha|http`.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "none" => Ok(Self::None),
            "aster-srv" => Ok(Self::AsterSrv),
            "gha" => Ok(Self::Gha),
            "http" => Ok(Self::Http),
            other => anyhow::bail!(
                "invalid {REMOTE_CACHE_ENV}={other:?}; expected aster-srv, gha, http, or none"
            ),
        }
    }

    /// `ASTER_REMOTE_CACHE`, or [`RemoteKind::AsterSrv`] when unset or empty.
    pub fn from_env() -> Result<Self> {
        match std::env::var(REMOTE_CACHE_ENV) {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            _ => Ok(Self::AsterSrv),
        }
    }
}

/// Synchronous blob and manifest store. HTTP backends use blocking clients.
pub trait CacheBackend: Send + Sync {
    /// Manifest for `key`, or `Ok(None)` on a miss.
    fn get_manifest(&self, key: &str) -> Result<Option<Manifest>>;

    /// Store `manifest`. Blobs it names must already be present.
    fn put_manifest(&self, manifest: &Manifest) -> Result<()>;

    /// Digests in `digests` that this backend does not have.
    fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>>;

    /// Write `digest` into `writer`. `Ok(false)` is a miss.
    fn get_blob(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool>;

    /// Store the bytes from `reader` under `digest`.
    fn put_blob(&self, digest: &Digest, reader: &mut dyn Read) -> Result<()>;
}

/// Process-lifetime remote that stops after repeated failures.
struct RemoteHandle {
    backend: Arc<dyn CacheBackend>,
    state: Mutex<RemoteState>,
    logged: AtomicBool,
}

struct RemoteState {
    failures: u32,
    disabled: bool,
}

impl RemoteHandle {
    fn new(backend: Arc<dyn CacheBackend>) -> Self {
        Self {
            backend,
            state: Mutex::new(RemoteState {
                failures: 0,
                disabled: false,
            }),
            logged: AtomicBool::new(false),
        }
    }

    fn active(&self) -> Option<&dyn CacheBackend> {
        let disabled = self
            .state
            .lock()
            .map(|state| state.disabled)
            .unwrap_or(true);
        if disabled {
            None
        } else {
            Some(self.backend.as_ref())
        }
    }

    fn succeed(&self) {
        if let Ok(mut state) = self.state.lock() {
            if !state.disabled {
                state.failures = 0;
            }
        }
    }

    /// Log the first error. Disable the remote once N consecutive failures
    /// have been observed. A later success does not turn it back on.
    fn fail(&self, error: &anyhow::Error) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.disabled {
            return;
        }
        if self
            .logged
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            eprintln!("[aster] Warning: remote cache failed, continuing locally: {error:#}");
        }
        state.failures += 1;
        if state.failures >= FAILURES_BEFORE_DISABLE {
            state.disabled = true;
        }
    }
}

/// Local store plus an optional remote.
///
/// Lookup checks local first. A remote hit is written into the local store
/// before it is returned, so restore always reads local blobs. Store writes
/// local first, then uploads blobs the remote is missing, then the manifest.
pub struct TieredCache {
    local: LocalStore,
    mode: CacheMode,
    remote: Option<RemoteHandle>,
}

impl TieredCache {
    /// Local-only tier. Used when the remote is off or not configured.
    pub fn local_only(local: LocalStore) -> Self {
        Self {
            local,
            mode: CacheMode::ReadWrite,
            remote: None,
        }
    }

    /// Tier with an explicit remote. `mode` still decides whether it is used.
    pub fn with_remote(local: LocalStore, mode: CacheMode, remote: Arc<dyn CacheBackend>) -> Self {
        let remote = if mode == CacheMode::Off {
            None
        } else {
            Some(RemoteHandle::new(remote))
        };
        Self {
            local,
            mode,
            remote,
        }
    }

    /// The local tier restore and pack already use.
    pub fn local_store(&self) -> &LocalStore {
        &self.local
    }

    /// Whether a remote is configured and has not been disabled.
    pub fn remote_enabled(&self) -> bool {
        self.remote
            .as_ref()
            .is_some_and(|remote| remote.active().is_some())
    }

    /// Manifest for `key`.
    ///
    /// Local hit wins. A remote hit is downloaded (missing blobs, then the
    /// manifest) into [`LocalStore`] and returned from there. Remote errors
    /// are a miss. Local I/O errors are returned.
    pub fn lookup(&self, key: &str) -> Result<Option<Manifest>> {
        if let Some(manifest) = self.local.get_manifest(key)? {
            if self.local_has_blobs(&manifest) {
                return Ok(Some(manifest));
            }
        }
        if !self.mode.reads() {
            return Ok(None);
        }
        let Some(remote) = self.remote.as_ref().and_then(|remote| remote.active()) else {
            return Ok(None);
        };
        match self.download(remote, key) {
            Ok(manifest) => Ok(manifest),
            Err(error) => {
                if let Some(handle) = &self.remote {
                    handle.fail(&error);
                }
                Ok(None)
            }
        }
    }

    /// Write blobs (already in the local store) and `manifest` locally, then
    /// upload. Read and off modes do not call remote `put_*`. A remote
    /// upload error does not fail the local store.
    pub fn store(&self, manifest: &Manifest) -> Result<()> {
        self.local.put_manifest(manifest)?;
        if !self.mode.writes() {
            return Ok(());
        }
        let Some(remote) = self.remote.as_ref().and_then(|remote| remote.active()) else {
            return Ok(());
        };
        if let Err(error) = self.upload(remote, manifest) {
            if let Some(handle) = &self.remote {
                handle.fail(&error);
            }
        }
        Ok(())
    }

    fn local_has_blobs(&self, manifest: &Manifest) -> bool {
        manifest
            .blob_refs()
            .iter()
            .all(|digest| self.local.has_blob(digest))
    }

    fn download(&self, remote: &dyn CacheBackend, key: &str) -> Result<Option<Manifest>> {
        let Some(manifest) = remote.get_manifest(key)? else {
            if let Some(handle) = &self.remote {
                handle.succeed();
            }
            return Ok(None);
        };
        let missing = remote.find_missing(&manifest.blob_refs())?;
        if !missing.is_empty() {
            anyhow::bail!("remote manifest {key} is missing {} blob(s)", missing.len());
        }
        for digest in manifest.blob_refs() {
            if self.local.has_blob(&digest) {
                continue;
            }
            // Spool to disk: a blob can be larger than memory.
            let mut spool = self.local.spool()?;
            if !remote.get_blob(&digest, &mut spool)? {
                anyhow::bail!(
                    "remote blob {} disappeared during download",
                    digest.as_str()
                );
            }
            self.local.put_blob_file(&digest, &mut spool)?;
        }
        self.local.put_manifest(&manifest)?;
        if let Some(handle) = &self.remote {
            handle.succeed();
        }
        self.local.get_manifest(key)
    }

    fn upload(&self, remote: &dyn CacheBackend, manifest: &Manifest) -> Result<()> {
        let refs = manifest.blob_refs();
        let missing = remote.find_missing(&refs)?;
        for digest in missing {
            // Decompress to a spool file rather than memory, then stream it.
            let mut spool = self.local.spool()?;
            self.local
                .get_blob(&digest, &mut spool)
                .with_context(|| format!("local blob {} missing before upload", digest.as_str()))?;
            spool
                .seek(SeekFrom::Start(0))
                .context("Failed to rewind blob spool")?;
            remote.put_blob(&digest, &mut spool)?;
        }
        remote.put_manifest(manifest)?;
        if let Some(handle) = &self.remote {
            handle.succeed();
        }
        Ok(())
    }
}

/// Build the remote for this process.
///
/// `none`, `gha`, and `http` stay local until their clients exist. `aster-srv`
/// stays local too: its client is another task. When `ASTER_SRV_URL` is set
/// and aster-srv was selected, one warning is printed so a configured gateway
/// is not a silent no-op.
pub fn select_remote(
    kind: RemoteKind,
    aster_srv_url: Option<&str>,
) -> Option<Arc<dyn CacheBackend>> {
    let url_set = aster_srv_url.is_some_and(|url| !url.trim().is_empty());
    if kind == RemoteKind::AsterSrv && url_set {
        eprintln!(
            "[aster] Warning: {ASTER_SRV_URL_ENV} is set but the aster-srv cache client is not available; using the local cache only"
        );
    }
    None
}

/// Remote kind and mode from the environment, then [`select_remote`].
pub fn tier_from_env(cache_dir: impl AsRef<Path>) -> Result<TieredCache> {
    let mode = CacheMode::from_env()?;
    let kind = RemoteKind::from_env()?;
    let local = LocalStore::new(cache_dir.as_ref().to_path_buf());
    if mode == CacheMode::Off || kind == RemoteKind::None {
        return Ok(TieredCache::local_only(local));
    }
    let url = std::env::var(ASTER_SRV_URL_ENV).ok();
    match select_remote(kind, url.as_deref()) {
        Some(remote) => Ok(TieredCache::with_remote(local, mode, remote)),
        None => Ok(TieredCache {
            local,
            mode,
            remote: None,
        }),
    }
}

/// In-memory [`CacheBackend`] for tests. Records `put_*` calls.
#[derive(Clone, Default)]
pub struct InMemoryBackend {
    inner: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    manifests: HashMap<String, Manifest>,
    blobs: HashMap<String, Vec<u8>>,
    fail_remaining: u32,
    puts: u32,
    gets: u32,
}

impl InMemoryBackend {
    /// Empty backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail the next `count` calls, then succeed.
    pub fn fail_next(&self, count: u32) {
        self.inner.lock().expect("backend lock").fail_remaining = count;
    }

    /// How many `put_manifest` and `put_blob` calls were attempted.
    pub fn put_calls(&self) -> u32 {
        self.inner.lock().expect("backend lock").puts
    }

    /// How many read calls were attempted.
    pub fn get_calls(&self) -> u32 {
        self.inner.lock().expect("backend lock").gets
    }

    fn before_call(state: &mut MemoryState, kind: CallKind) -> Result<()> {
        match kind {
            CallKind::Get => state.gets += 1,
            CallKind::Put => state.puts += 1,
        }
        if state.fail_remaining > 0 {
            state.fail_remaining -= 1;
            anyhow::bail!("injected remote failure");
        }
        Ok(())
    }
}

enum CallKind {
    Get,
    Put,
}

impl CacheBackend for InMemoryBackend {
    fn get_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        let mut state = self.inner.lock().expect("backend lock");
        Self::before_call(&mut state, CallKind::Get)?;
        Ok(state.manifests.get(key).cloned())
    }

    fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
        let mut state = self.inner.lock().expect("backend lock");
        Self::before_call(&mut state, CallKind::Put)?;
        state
            .manifests
            .insert(manifest.key.clone(), manifest.clone());
        Ok(())
    }

    fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>> {
        let mut state = self.inner.lock().expect("backend lock");
        Self::before_call(&mut state, CallKind::Get)?;
        Ok(digests
            .iter()
            .filter(|digest| !state.blobs.contains_key(digest.as_str()))
            .cloned()
            .collect())
    }

    fn get_blob(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool> {
        let mut state = self.inner.lock().expect("backend lock");
        Self::before_call(&mut state, CallKind::Get)?;
        match state.blobs.get(digest.as_str()) {
            Some(bytes) => {
                writer.write_all(bytes)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn put_blob(&self, digest: &Digest, reader: &mut dyn Read) -> Result<()> {
        let mut state = self.inner.lock().expect("backend lock");
        Self::before_call(&mut state, CallKind::Put)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        state.blobs.insert(digest.as_str().to_string(), bytes);
        Ok(())
    }
}

/// `put_blob` takes `dyn Read`, so a backend can stream without buffering first.
#[cfg(test)]
fn _read_object_is_accepted(backend: &dyn CacheBackend, digest: &Digest, reader: &mut dyn Read) {
    let _ = backend.put_blob(digest, reader);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::artifact::{OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION};

    fn manifest(key: &str, blob: Option<&Digest>) -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: key.to_string(),
            target: "//app:build".to_string(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-17T00:00:00Z".to_string(),
            outputs: blob
                .map(|digest| OutputEntry {
                    path: "out/file.txt".into(),
                    kind: OutputKind::File,
                    digest: Some(digest.clone()),
                    mode: 0o644,
                    target: None,
                })
                .into_iter()
                .collect(),
            log: None,
            aster_version: "0.21.0".to_string(),
        }
    }

    fn seeded_remote(key: &str, bytes: &[u8]) -> (InMemoryBackend, Manifest, Digest) {
        let digest = Digest::of_bytes(bytes);
        let stored = manifest(key, Some(&digest));
        let remote = InMemoryBackend::new();
        remote
            .put_blob(&digest, &mut bytes.to_vec().as_slice())
            .unwrap();
        remote.put_manifest(&stored).unwrap();
        (remote, stored, digest)
    }

    #[test]
    fn remote_hit_populates_local_then_restores_from_local() {
        let tmp = tempfile::tempdir().unwrap();
        let key = "ab".repeat(32);
        let (remote, stored, digest) = seeded_remote(&key, b"artifact-bytes");
        let puts_after_seed = remote.put_calls();
        let tier = TieredCache::with_remote(
            LocalStore::new(tmp.path().to_path_buf()),
            CacheMode::ReadWrite,
            Arc::new(remote.clone()),
        );

        let hit = tier.lookup(&key).unwrap().expect("remote hit");
        assert_eq!(hit, stored);
        assert!(tier.local_store().has_blob(&digest));
        let mut restored = Vec::new();
        tier.local_store().get_blob(&digest, &mut restored).unwrap();
        assert_eq!(restored, b"artifact-bytes");

        remote.fail_next(10);
        let again = tier.lookup(&key).unwrap().expect("local hit");
        assert_eq!(again.key, key);
        assert_eq!(remote.put_calls(), puts_after_seed);
    }

    #[test]
    fn large_blob_round_trips_through_spool_files() {
        // Larger than the 64 KiB copy buffer, so both directions stream in
        // several chunks through the on-disk spool.
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let key = "ef".repeat(32);
        let (remote, _stored, digest) = seeded_remote(&key, &bytes);

        let down = tempfile::tempdir().unwrap();
        let tier = TieredCache::with_remote(
            LocalStore::new(down.path().to_path_buf()),
            CacheMode::ReadWrite,
            Arc::new(remote.clone()),
        );
        tier.lookup(&key).unwrap().expect("remote hit");
        let mut restored = Vec::new();
        tier.local_store().get_blob(&digest, &mut restored).unwrap();
        assert_eq!(restored, bytes);

        let up = tempfile::tempdir().unwrap();
        let local = LocalStore::new(up.path().to_path_buf());
        local.put_blob(&digest, bytes.as_slice()).unwrap();
        let target = InMemoryBackend::new();
        let uploader =
            TieredCache::with_remote(local, CacheMode::ReadWrite, Arc::new(target.clone()));
        uploader.store(&manifest(&key, Some(&digest))).unwrap();
        let mut uploaded = Vec::new();
        assert!(target.get_blob(&digest, &mut uploaded).unwrap());
        assert_eq!(uploaded, bytes);

        // Spool files are unnamed and leave nothing behind in the cache dir.
        for dir in [down.path(), up.path()] {
            let stray: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_file())
                .filter(|entry| entry.file_name() != "cache.lock")
                .collect();
            assert!(stray.is_empty(), "unexpected files: {stray:?}");
        }
    }

    #[test]
    fn upload_writes_missing_blobs_then_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let local = LocalStore::new(tmp.path().to_path_buf());
        let digest = Digest::of_bytes(b"only-local");
        local.put_blob(&digest, &b"only-local"[..]).unwrap();
        let key = "cd".repeat(32);
        let stored = manifest(&key, Some(&digest));
        let remote = InMemoryBackend::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let tracking = OrderBackend {
            inner: remote.clone(),
            order: Arc::clone(&order),
        };
        let tier = TieredCache::with_remote(local, CacheMode::ReadWrite, Arc::new(tracking));

        tier.store(&stored).unwrap();

        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["put_blob", "put_manifest"]
        );
        let mut downloaded = Vec::new();
        assert!(remote.get_blob(&digest, &mut downloaded).unwrap());
        assert_eq!(downloaded, b"only-local");
        assert!(remote.get_manifest(&key).unwrap().is_some());
    }

    #[test]
    fn upload_skips_blobs_the_remote_already_has() {
        let tmp = tempfile::tempdir().unwrap();
        let local = LocalStore::new(tmp.path().to_path_buf());
        let digest = Digest::of_bytes(b"shared");
        local.put_blob(&digest, &b"shared"[..]).unwrap();
        let key = "ee".repeat(32);
        let stored = manifest(&key, Some(&digest));
        let remote = InMemoryBackend::new();
        remote
            .put_blob(&digest, &mut b"shared".to_vec().as_slice())
            .unwrap();
        let puts = remote.put_calls();
        let tier = TieredCache::with_remote(local, CacheMode::ReadWrite, Arc::new(remote.clone()));

        tier.store(&stored).unwrap();

        assert_eq!(remote.put_calls(), puts + 1);
        assert!(remote.get_manifest(&key).unwrap().is_some());
    }

    #[test]
    fn write_mode_downloads_a_hit_but_store_does_not_skip_upload() {
        let tmp = tempfile::tempdir().unwrap();
        let key = "55".repeat(32);
        let (remote, stored, digest) = seeded_remote(&key, b"write-mode-hit");
        let tier = TieredCache::with_remote(
            LocalStore::new(tmp.path().to_path_buf()),
            CacheMode::Write,
            Arc::new(remote.clone()),
        );

        let hit = tier
            .lookup(&key)
            .unwrap()
            .expect("write mode still downloads");
        assert_eq!(hit, stored);
        assert!(tier.local_store().has_blob(&digest));

        let extra = Digest::of_bytes(b"uploaded");
        tier.local_store()
            .put_blob(&extra, &b"uploaded"[..])
            .unwrap();
        let puts = remote.put_calls();
        tier.store(&manifest(&"66".repeat(32), Some(&extra)))
            .unwrap();
        assert!(remote.put_calls() > puts);
    }

    #[test]
    fn read_mode_never_calls_put() {
        let tmp = tempfile::tempdir().unwrap();
        let local = LocalStore::new(tmp.path().to_path_buf());
        let digest = Digest::of_bytes(b"local-only");
        local.put_blob(&digest, &b"local-only"[..]).unwrap();
        let key = "ff".repeat(32);
        let stored = manifest(&key, Some(&digest));
        let remote = InMemoryBackend::new();
        let tier = TieredCache::with_remote(local, CacheMode::Read, Arc::new(remote.clone()));

        tier.store(&stored).unwrap();

        assert_eq!(remote.put_calls(), 0);
        assert!(remote.get_manifest(&key).unwrap().is_none());
        assert!(tier.local_store().get_manifest(&key).unwrap().is_some());
    }

    #[test]
    fn off_mode_never_contacts_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let key = "11".repeat(32);
        let (remote, _, _) = seeded_remote(&key, b"remote-bytes");
        let gets = remote.get_calls();
        let tier = TieredCache::with_remote(
            LocalStore::new(tmp.path().to_path_buf()),
            CacheMode::Off,
            Arc::new(remote.clone()),
        );

        assert!(!tier.remote_enabled());
        assert!(tier.lookup(&key).unwrap().is_none());
        assert_eq!(remote.get_calls(), gets);
        assert_eq!(remote.put_calls(), 2);
    }

    #[test]
    fn remote_failures_disable_remote_for_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = InMemoryBackend::new();
        remote.fail_next(100);
        let tier = TieredCache::with_remote(
            LocalStore::new(tmp.path().to_path_buf()),
            CacheMode::ReadWrite,
            Arc::new(remote.clone()),
        );
        let key = "22".repeat(32);

        for _ in 0..FAILURES_BEFORE_DISABLE {
            assert!(tier.lookup(&key).unwrap().is_none());
        }
        let calls = remote.get_calls();
        assert!(!tier.remote_enabled());
        assert!(tier.lookup(&key).unwrap().is_none());
        assert_eq!(remote.get_calls(), calls);

        let digest = Digest::of_bytes(b"still-local");
        tier.local_store()
            .put_blob(&digest, &b"still-local"[..])
            .unwrap();
        tier.store(&manifest(&"33".repeat(32), Some(&digest)))
            .unwrap();
        assert_eq!(remote.put_calls(), 0);
    }

    #[test]
    fn a_successful_call_resets_the_failure_streak() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = InMemoryBackend::new();
        remote.fail_next(2);
        let tier = TieredCache::with_remote(
            LocalStore::new(tmp.path().to_path_buf()),
            CacheMode::ReadWrite,
            Arc::new(remote.clone()),
        );
        let key = "44".repeat(32);

        assert!(tier.lookup(&key).unwrap().is_none());
        assert!(tier.lookup(&key).unwrap().is_none());
        assert!(tier.lookup(&key).unwrap().is_none());
        assert!(tier.remote_enabled());
        remote.fail_next(2);
        assert!(tier.lookup(&key).unwrap().is_none());
        assert!(tier.lookup(&key).unwrap().is_none());
        assert!(tier.remote_enabled());
    }

    #[test]
    fn cache_mode_and_remote_kind_parse() {
        assert_eq!(CacheMode::parse("off").unwrap(), CacheMode::Off);
        assert_eq!(CacheMode::parse("read").unwrap(), CacheMode::Read);
        assert_eq!(CacheMode::parse("write").unwrap(), CacheMode::Write);
        assert_eq!(CacheMode::parse(" rw ").unwrap(), CacheMode::ReadWrite);
        assert!(CacheMode::parse("nope").is_err());
        assert_eq!(RemoteKind::parse("none").unwrap(), RemoteKind::None);
        assert_eq!(
            RemoteKind::parse("aster-srv").unwrap(),
            RemoteKind::AsterSrv
        );
        assert_eq!(RemoteKind::parse("gha").unwrap(), RemoteKind::Gha);
        assert_eq!(RemoteKind::parse("http").unwrap(), RemoteKind::Http);
        assert!(RemoteKind::parse("s3").is_err());
        assert!(select_remote(RemoteKind::AsterSrv, Some("https://example.test")).is_none());
        assert!(select_remote(RemoteKind::None, None).is_none());
    }

    /// Records upload method names so the blob-then-manifest order is asserted.
    struct OrderBackend {
        inner: InMemoryBackend,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl CacheBackend for OrderBackend {
        fn get_manifest(&self, key: &str) -> Result<Option<Manifest>> {
            self.inner.get_manifest(key)
        }

        fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
            self.order.lock().unwrap().push("put_manifest");
            self.inner.put_manifest(manifest)
        }

        fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>> {
            self.inner.find_missing(digests)
        }

        fn get_blob(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool> {
            self.inner.get_blob(digest, writer)
        }

        fn put_blob(&self, digest: &Digest, reader: &mut dyn Read) -> Result<()> {
            self.order.lock().unwrap().push("put_blob");
            self.inner.put_blob(digest, reader)
        }
    }
}
