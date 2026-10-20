//! GitHub Actions [`CacheBackend`].
//!
//! One manifest entry per target:
//! `aster-v<schema>-ac-<os>-<arch>-<taskkey>`.
//!
//! A blob whose zstd frame is at least 1 MiB is its own entry,
//! `aster-v<schema>-cas-<digest>`. Smaller frames wait in memory and are
//! uploaded once, when the run finishes, as an uncompressed tar of
//! `<digest>` members:
//! `aster-v<schema>-pack-<os>-<arch>-<sha256 of the contained digests>`.
//! The member set is sealed when the first manifest is uploaded, so the
//! key is known and immutable. A small blob staged after that is its own
//! entry. Each manifest records the pack key for every blob it bundled.
//!
//! Keys are immutable. [`CacheBackend::find_missing`] only looks keys up;
//! it never calls `CreateCacheEntry`. A create that reports the key already
//! exists does not upload. After repeated 429s, later writes return success
//! without a request, and one line names the 429 count and the entries
//! created before the stop.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use sha2::{Digest as Sha256Digest, Sha256};

use super::client::GhaCacheClient;
use crate::cache::artifact::{Digest, Manifest, PackRef};
use crate::cache::backend::CacheBackend;
use crate::cache::hasher::encode_hex;
use crate::cache::CACHE_SCHEMA_VERSION;

/// A blob of at least this many uncompressed bytes is its own entry.
/// Smaller blobs wait for the per-run pack. The pack stores the zstd frame.
pub const PACK_THRESHOLD: u64 = 1024 * 1024;

/// One 429 that survives the client retry budget stops writes for the run.
/// The client already retried; another budget would keep creating entries
/// against a store that is refusing them.
const RATE_LIMITS_BEFORE_STOP: u64 = 1;

/// Version sent with every Twirp call. Independent of the key prefix so a
/// schema bump does not collide with an older entry of the same key.
const VERSION: &str = "aster";

/// End-of-run counters. Printed as one line and stored as `cache_summary`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheSummary {
    pub remote_hits: u64,
    pub remote_misses: u64,
    pub entries_created: u64,
    pub rate_limited: u64,
}

struct Staged {
    frame: Vec<u8>,
}

struct State {
    /// Small blobs waiting for the pack, in first-seen order.
    order: Vec<Digest>,
    frames: HashMap<String, Staged>,
    /// Set when the first manifest names a staged blob. Later blobs cannot
    /// join: the key is a hash of the members already recorded.
    pack_key: Option<String>,
    /// Digests the current pack contains. Kept after upload so a manifest
    /// stored in the same run still records the pack.
    pack_members: HashSet<String>,
    /// Pack key to the digests it holds, so `get_blob` can find a bundled blob.
    known_packs: HashMap<String, HashSet<String>>,
}

/// [`CacheBackend`] selected by `ASTER_REMOTE_CACHE=gha`.
pub struct GhaBackend {
    client: GhaCacheClient,
    os: String,
    arch: String,
    /// When false, every blob is its own entry. The entry-count test uses
    /// this to show bundling is what keeps the count at 51.
    bundle: bool,
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    created: AtomicU64,
    rate_limited: AtomicU64,
    writes_stopped: AtomicBool,
    rate_limit_logged: AtomicBool,
    /// `CreateCacheEntry` calls, including ones that found an existing key.
    creates: AtomicU64,
}

impl GhaBackend {
    /// Backend for this host. Small blobs are bundled.
    pub fn new(client: GhaCacheClient) -> Self {
        Self::with_platform(client, std::env::consts::OS, std::env::consts::ARCH, true)
    }

    /// `bundle` is false when the entry-count test turns packing off.
    pub fn with_platform(client: GhaCacheClient, os: &str, arch: &str, bundle: bool) -> Self {
        Self {
            client,
            os: os.to_string(),
            arch: arch.to_string(),
            bundle,
            state: Mutex::new(State {
                order: Vec::new(),
                frames: HashMap::new(),
                pack_key: None,
                pack_members: HashSet::new(),
                known_packs: HashMap::new(),
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            created: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            writes_stopped: AtomicBool::new(false),
            rate_limit_logged: AtomicBool::new(false),
            creates: AtomicU64::new(0),
        }
    }

    /// `ACTIONS_RESULTS_URL` and `ACTIONS_RUNTIME_TOKEN`. `None` when either
    /// is missing or the URL is not `https://`.
    pub fn from_env() -> Option<Self> {
        GhaCacheClient::from_env().map(Self::new)
    }

    /// Counters for the summary line and `cache_summary`.
    pub fn summary(&self) -> CacheSummary {
        CacheSummary {
            remote_hits: self.hits.load(Ordering::Relaxed),
            remote_misses: self.misses.load(Ordering::Relaxed),
            entries_created: self.created.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
        }
    }

    /// `CreateCacheEntry` calls made by this backend.
    pub fn create_calls(&self) -> u64 {
        self.creates.load(Ordering::Relaxed)
    }

    /// Whether repeated 429s have stopped writes.
    pub fn writes_stopped(&self) -> bool {
        self.writes_stopped.load(Ordering::Acquire)
    }

    /// Upload the staged pack. Called at the end of the run, and also before
    /// a manifest is uploaded so a reader never sees a manifest whose pack
    /// does not exist yet. A second call with an empty stage is a no-op.
    pub fn finish(&self) -> Result<()> {
        if self.writes_stopped() {
            return Ok(());
        }
        let (key, frames) = {
            let state = self.state.lock().expect("gha cache state");
            if state.order.is_empty() {
                return Ok(());
            }
            let members: Vec<Digest> = state.order.clone();
            let key = state
                .pack_key
                .clone()
                .unwrap_or_else(|| pack_key(&self.os, &self.arch, &members));
            let frames: Vec<(Digest, Vec<u8>)> = state
                .order
                .iter()
                .filter(|digest| {
                    state.pack_members.is_empty()
                        || state.pack_members.contains(digest.as_str())
                })
                .filter_map(|digest| {
                    state
                        .frames
                        .get(digest.as_str())
                        .map(|staged| (digest.clone(), staged.frame.clone()))
                })
                .collect();
            (key, frames)
        };
        if frames.is_empty() {
            return Ok(());
        }
        self.upload_entry(&key, &pack_archive(&frames)?)?;
        let mut state = self.state.lock().expect("gha cache state");
        let pending = state.pack_key.clone();
        for (digest, _) in &frames {
            state.frames.remove(digest.as_str());
            state.order.retain(|staged| staged != digest);
        }
        state.pack_key = Some(key.clone());
        if let Some(pending) = pending {
            if pending != key {
                if let Some(members) = state.known_packs.remove(&pending) {
                    state.known_packs.insert(key, members);
                }
            }
        }
        Ok(())
    }

    fn manifest_key(&self, task_key: &str) -> String {
        format!(
            "aster-v{CACHE_SCHEMA_VERSION}-ac-{}-{}-{task_key}",
            self.os, self.arch
        )
    }

    fn cas_key(digest: &Digest) -> String {
        format!("aster-v{CACHE_SCHEMA_VERSION}-cas-{}", digest.as_str())
    }

    fn remember_manifest(&self, manifest: &Manifest) {
        let mut state = self.state.lock().expect("gha cache state");
        for pack in &manifest.packs {
            state
                .known_packs
                .entry(pack.pack.clone())
                .or_default()
                .insert(pack.digest.as_str().to_string());
        }
    }

    /// Reserve `key` and upload `bytes` unless the key already exists.
    fn upload_entry(&self, key: &str, bytes: &[u8]) -> Result<()> {
        if self.writes_stopped() {
            return Ok(());
        }
        self.creates.fetch_add(1, Ordering::Relaxed);
        let created = match self.client.create(key, VERSION) {
            Ok(created) => created,
            Err(error) => {
                self.note_write_error(&error);
                return Err(error);
            }
        };
        // An existing key has no upload URL. Do not upload or finalize:
        // that would be an attempt to overwrite an immutable key.
        if created.already_exists || created.upload_url.is_none() {
            return Ok(());
        }
        let url = created.upload_url.as_deref().unwrap_or_default();
        if let Err(error) = self
            .client
            .upload(url, io::Cursor::new(bytes), bytes.len() as u64)
        {
            self.note_write_error(&error);
            return Err(error);
        }
        if let Err(error) = self.client.finalize(key, VERSION, bytes.len() as u64) {
            self.note_write_error(&error);
            return Err(error);
        }
        self.created.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn lookup_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.client.lookup(key, VERSION) {
            Ok(None) => Ok(None),
            Ok(Some(url)) => {
                let mut bytes = Vec::new();
                self.client
                    .download(&url, &mut bytes)
                    .context("download cache entry")?;
                Ok(Some(bytes))
            }
            Err(error) => {
                self.note_write_error(&error);
                Err(error)
            }
        }
    }

    fn note_write_error(&self, error: &anyhow::Error) {
        if is_rate_limited(error) {
            self.note_rate_limit();
        }
    }

    fn note_rate_limit(&self) {
        let count = self.rate_limited.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= RATE_LIMITS_BEFORE_STOP {
            self.writes_stopped.store(true, Ordering::Release);
            if self
                .rate_limit_logged
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                eprintln!(
                    "[aster] Warning: GitHub Actions cache rate limited ({count} repeated 429s, {} entries created); remote writes stopped for this run",
                    self.created.load(Ordering::Relaxed)
                );
            }
        }
    }

    fn stage_frame(&self, digest: &Digest, frame: Vec<u8>) -> Result<()> {
        if !self.bundle {
            return self.upload_entry(&Self::cas_key(digest), &frame);
        }
        let mut state = self.state.lock().expect("gha cache state");
        if state.pack_key.is_some() && !state.pack_members.contains(digest.as_str()) {
            drop(state);
            return self.upload_entry(&Self::cas_key(digest), &frame);
        }
        if !state.frames.contains_key(digest.as_str()) {
            state.order.push(digest.clone());
            state
                .frames
                .insert(digest.as_str().to_string(), Staged { frame });
        }
        Ok(())
    }

    /// Close the pack member set and return its hashed key.
    ///
    /// Called before the first manifest upload. Blobs staged after this
    /// are their own entries: the key is a hash of the members, and a
    /// manifest already uploaded cannot change.
    fn seal_pack(&self) -> Option<String> {
        let mut state = self.state.lock().expect("gha cache state");
        if state.order.is_empty() && state.pack_key.is_none() {
            return None;
        }
        if state.pack_key.is_none() {
            let members = state.order.clone();
            state.pack_members = members
                .iter()
                .map(|digest| digest.as_str().to_string())
                .collect();
            state.pack_key = Some(pack_key(&self.os, &self.arch, &members));
        }
        state.pack_key.clone()
    }

    fn write_packed(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool> {
        let packs: Vec<String> = {
            let state = self.state.lock().expect("gha cache state");
            state
                .known_packs
                .iter()
                .filter(|(_, members)| members.contains(digest.as_str()))
                .map(|(key, _)| key.clone())
                .collect()
        };
        for pack in packs {
            let Some(archive) = self.lookup_bytes(&pack)? else {
                continue;
            };
            if let Some(frame) = unpack_member(&archive, digest.as_str())? {
                decode_frame(&frame, writer)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl CacheBackend for GhaBackend {
    fn get_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        match self.lookup_bytes(&self.manifest_key(key))? {
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
            Some(bytes) => match Manifest::decode(&bytes) {
                Some(manifest) => {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    self.remember_manifest(&manifest);
                    Ok(Some(manifest))
                }
                None => {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    Ok(None)
                }
            },
        }
    }

    fn put_manifest(&self, manifest: &Manifest) -> Result<()> {
        let pack = self.seal_pack().filter(|_| {
            manifest.blob_refs().iter().any(|digest| {
                self.state
                    .lock()
                    .expect("gha cache state")
                    .pack_members
                    .contains(digest.as_str())
            })
        });
        let members = self
            .state
            .lock()
            .expect("gha cache state")
            .pack_members
            .clone();
        let mut stored = manifest.clone();
        if let Some(pack) = pack {
            stored.packs = manifest
                .blob_refs()
                .into_iter()
                .filter(|digest| members.contains(digest.as_str()))
                .map(|digest| PackRef {
                    digest,
                    pack: pack.clone(),
                })
                .collect();
        }
        self.remember_manifest(&stored);
        self.upload_entry(&self.manifest_key(&manifest.key), &stored.encode())
    }

    fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>> {
        let mut missing = Vec::new();
        for digest in digests {
            let staged = {
                let state = self.state.lock().expect("gha cache state");
                state.frames.contains_key(digest.as_str())
                    || state.pack_members.contains(digest.as_str())
            };
            if staged {
                continue;
            }
            if self.write_would_find_pack(digest)? {
                continue;
            }
            // Lookup only. Create reserves a key, which is a write.
            match self.client.lookup(&Self::cas_key(digest), VERSION) {
                Ok(Some(_)) => {}
                Ok(None) => missing.push(digest.clone()),
                Err(error) => {
                    self.note_write_error(&error);
                    return Err(error);
                }
            }
        }
        Ok(missing)
    }

    fn get_blob(&self, digest: &Digest, writer: &mut dyn Write) -> Result<bool> {
        if let Some(frame) = self.lookup_bytes(&Self::cas_key(digest))? {
            decode_frame(&frame, writer)?;
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        if self.write_packed(digest, writer)? {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    fn put_blob(&self, digest: &Digest, reader: &mut dyn Read) -> Result<()> {
        let mut plain = Vec::new();
        reader.read_to_end(&mut plain).context("read blob")?;
        let hashed = Digest::of_bytes(&plain);
        if hashed.as_str() != digest.as_str() {
            anyhow::bail!(
                "blob digest mismatch: expected {}, hashed {}",
                digest.as_str(),
                hashed.as_str()
            );
        }
        let frame = zstd::encode_all(plain.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
            .context("compress blob")?;
        // The threshold is the blob size, not the frame size. A compressible
        // blob of 1 MiB is still its own entry; only a smaller blob waits
        // for the pack. The frame is what the pack stores.
        if plain.len() as u64 >= PACK_THRESHOLD {
            return self.upload_entry(&Self::cas_key(digest), &frame);
        }
        self.stage_frame(digest, frame)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl GhaBackend {
    /// A digest known to live in a pack this process has already read.
    fn write_would_find_pack(&self, digest: &Digest) -> Result<bool> {
        let packs: Vec<String> = {
            let state = self.state.lock().expect("gha cache state");
            state
                .known_packs
                .iter()
                .filter(|(_, members)| members.contains(digest.as_str()))
                .map(|(key, _)| key.clone())
                .collect()
        };
        Ok(!packs.is_empty())
    }
}

fn is_rate_limited(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    text.contains(" 429") || text.contains("failed: 429")
}

fn pack_key(os: &str, arch: &str, digests: &[Digest]) -> String {
    format!(
        "aster-v{CACHE_SCHEMA_VERSION}-pack-{os}-{arch}-{}",
        digest_set_hash(digests)
    )
}

fn digest_set_hash(digests: &[Digest]) -> String {
    let mut sorted: Vec<&str> = digests.iter().map(Digest::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = Sha256::new();
    for digest in &sorted {
        Sha256Digest::update(&mut hasher, digest.as_bytes());
        Sha256Digest::update(&mut hasher, b"\n");
    }
    encode_hex(Sha256Digest::finalize(hasher).as_ref())
}

/// Uncompressed ustar. Each member is named by its digest and holds the
/// zstd frame.
fn pack_archive(frames: &[(Digest, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (digest, frame) in frames {
        write_ustar(&mut out, digest.as_str(), frame)?;
    }
    out.extend_from_slice(&[0_u8; 1024]);
    Ok(out)
}

fn write_ustar(out: &mut Vec<u8>, name: &str, bytes: &[u8]) -> Result<()> {
    if name.len() > 100 {
        anyhow::bail!("tar member name longer than 100 bytes: {name}");
    }
    let mut header = [0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], bytes.len() as u64);
    write_octal(&mut header[136..148], 0);
    header[156] = b'0';
    header[257..262].copy_from_slice(b"ustar");
    header[262] = 0;
    header[263] = b'0';
    header[264] = b'0';
    header[148..156].fill(b' ');
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    write_octal(&mut header[148..156], u64::from(sum));
    out.extend_from_slice(&header);
    out.extend_from_slice(bytes);
    let pad = (512 - (bytes.len() % 512)) % 512;
    out.extend_from_slice(&vec![0_u8; pad]);
    Ok(())
}

fn write_octal(field: &mut [u8], value: u64) {
    let text = format!("{value:o}");
    let width = field.len() - 1;
    field[..width].fill(b'0');
    let start = width.saturating_sub(text.len());
    let len = text.len().min(width);
    field[start..start + len].copy_from_slice(&text.as_bytes()[..len]);
    field[field.len() - 1] = 0;
}

fn unpack_member(archive: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    let mut offset = 0;
    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            return Ok(None);
        }
        let member = header_name(header);
        let size = parse_octal(&header[124..136]).context("tar member size")?;
        offset += 512;
        let end = offset + size;
        if end > archive.len() {
            anyhow::bail!("tar member {member} extends past the archive");
        }
        if member == name {
            return Ok(Some(archive[offset..end].to_vec()));
        }
        let pad = (512 - (size % 512)) % 512;
        offset = end + pad;
    }
    Ok(None)
}

fn header_name(header: &[u8]) -> String {
    let end = header[..100]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(100);
    String::from_utf8_lossy(&header[..end]).into_owned()
}

fn parse_octal(field: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(field)
        .context("tar octal field")?
        .trim_matches(|ch: char| ch == '\0' || ch == ' ');
    if text.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(text, 8).with_context(|| format!("tar octal {text:?}"))
}

fn decode_frame(frame: &[u8], writer: &mut dyn Write) -> Result<()> {
    let plain = zstd::decode_all(frame).context("decompress blob")?;
    writer.write_all(&plain).context("write blob")?;
    Ok(())
}
