//! Content-addressed artifact manifests.
//!
//! An action cache entry is a small JSON manifest keyed by a task hash.
//! File contents live in the content-addressed store as blobs keyed by
//! [`Digest`] (sha256 of the uncompressed bytes).
//!
//! A target's output files are stored as one archive: a zstd-compressed tar
//! stream cut into chunks, each chunk a blob. The manifest then names only the
//! chunks in `outputs` (kind `archive_chunk`), so the server resolves a handful
//! of blobs per target instead of one per file, and keeps the real file list
//! in `archive`. A client that predates archives fails to parse the unknown
//! kind and treats the entry as a miss.

use serde::{Deserialize, Serialize};
use sha2::{Digest as ShaDigest, Sha256};
use std::io::{self, Read};
use std::path::{Component, Path};

use super::hasher::encode_hex;

/// Schema version written into every manifest. Decode rejects any other value.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Lowercase hex sha256 of uncompressed content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// Hash uncompressed bytes.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self::of_reader(bytes).expect("in-memory read cannot fail")
    }

    /// Hash uncompressed bytes from a reader.
    pub fn of_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            ShaDigest::update(&mut hasher, &buf[..n]);
        }
        Ok(Self(encode_hex(ShaDigest::finalize(hasher).as_ref())))
    }

    /// The lowercase hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap an already-computed lowercase hex digest.
    pub(crate) fn from_hex(hex: String) -> Self {
        Self(hex)
    }
}

/// What an output path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    File,
    Dir,
    Symlink,
    /// One chunk of the manifest's [`OutputArchive`]. Only appears in
    /// [`Manifest::outputs`] of an archived manifest. Older clients do not
    /// know this kind, so they cannot decode such a manifest.
    ArchiveChunk,
}

/// One restored output. `path` is project-relative and `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    pub path: String,
    pub kind: OutputKind,
    /// Content digest. Required for [`OutputKind::File`].
    pub digest: Option<Digest>,
    pub mode: u32,
    /// Symlink target. Set for [`OutputKind::Symlink`].
    pub target: Option<String>,
}

/// The output tree of a manifest whose file contents are one chunked archive.
///
/// `entries` is what `outputs` holds in an unarchived manifest. Every
/// [`OutputKind::File`] entry has its bytes in the archive, as the tar member
/// named by the entry's index in `entries`; directories and symlinks live only
/// here. The chunks are the manifest's `outputs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputArchive {
    pub entries: Vec<OutputEntry>,
}

/// A small blob stored inside a per-run pack entry rather than its own key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    /// Content digest of the blob.
    pub digest: Digest,
    /// Cache key of the pack entry that contains the blob's compressed bytes.
    pub pack: String,
}

/// Action-cache manifest for one target execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// CacheHasher hex key.
    pub key: String,
    /// Target address, e.g. `//apps/web:build`.
    pub target: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    /// RFC 3339 timestamp.
    pub created_at: String,
    pub outputs: Vec<OutputEntry>,
    pub log: Option<Digest>,
    pub aster_version: String,
    /// Pack entries that hold this manifest's small blobs. Empty for a
    /// manifest whose blobs each have their own cache entry. Absent in
    /// manifests written before packs existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<PackRef>,
    /// Set when the output files are stored as one chunked archive. Then
    /// `outputs` lists only the archive's chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<OutputArchive>,
}

impl Manifest {
    /// JSON encoding of this manifest.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest is always JSON-serializable")
    }

    /// Decode a manifest. `None` means a cache miss: invalid JSON, a different
    /// schema version, or a manifest that fails [`Self::validate`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let manifest: Self = serde_json::from_slice(bytes).ok()?;
        if manifest.schema != MANIFEST_SCHEMA_VERSION {
            return None;
        }
        manifest.validate().ok()?;
        Some(manifest)
    }

    /// Reject absolute paths, `..` components, file entries without a digest,
    /// symlink entries whose target is missing or escapes the project, and an
    /// archive whose chunk list is not the manifest's `outputs`.
    pub fn validate(&self) -> anyhow::Result<()> {
        match &self.archive {
            None => {
                for output in &self.outputs {
                    validate_entry(output)?;
                }
            }
            Some(archive) => {
                for (index, chunk) in self.outputs.iter().enumerate() {
                    if chunk.kind != OutputKind::ArchiveChunk
                        || chunk.digest.is_none()
                        || chunk.path != chunk_path(index)
                    {
                        anyhow::bail!("archive chunk {index} is malformed");
                    }
                }
                if self.outputs.is_empty() {
                    anyhow::bail!("archive has no chunks");
                }
                for entry in &archive.entries {
                    validate_entry(entry)?;
                }
                if !archive
                    .entries
                    .iter()
                    .any(|entry| entry.kind == OutputKind::File)
                {
                    anyhow::bail!("archive holds no files");
                }
            }
        }
        Ok(())
    }

    /// The tree a restore materializes: the archive's entries, else `outputs`.
    pub fn tree(&self) -> &[OutputEntry] {
        match &self.archive {
            Some(archive) => &archive.entries,
            None => &self.outputs,
        }
    }

    /// Chunk digests of the archive, in stream order. Empty without one.
    pub fn archive_chunks(&self) -> Vec<Digest> {
        if self.archive.is_none() {
            return Vec::new();
        }
        self.outputs
            .iter()
            .filter_map(|chunk| chunk.digest.clone())
            .collect()
    }

    /// Every blob this manifest references: output digests, then the log.
    pub fn blob_refs(&self) -> Vec<Digest> {
        let mut refs: Vec<Digest> = self
            .outputs
            .iter()
            .filter_map(|output| output.digest.clone())
            .collect();
        if let Some(log) = &self.log {
            refs.push(log.clone());
        }
        refs
    }
}

/// The `outputs` path of archive chunk `index`.
pub fn chunk_path(index: usize) -> String {
    format!("chunk/{index:06}")
}

fn validate_entry(output: &OutputEntry) -> anyhow::Result<()> {
    if !is_safe_relative_path(&output.path) {
        anyhow::bail!("unsafe output path: {}", output.path);
    }
    match output.kind {
        OutputKind::File if output.digest.is_none() => {
            anyhow::bail!("file output missing digest: {}", output.path);
        }
        OutputKind::Symlink => match &output.target {
            Some(target) if symlink_target_stays_in_project(&output.path, target) => {}
            _ => anyhow::bail!("unsafe symlink target for {}", output.path),
        },
        OutputKind::ArchiveChunk => {
            anyhow::bail!("archive chunk outside an archive: {}", output.path)
        }
        _ => {}
    }
    Ok(())
}

/// Project-relative, `/`-separated path: only normal components, so prefixes, roots, and `..` are rejected.
/// Backslash and drive prefixes are rejected as text because Unix `Path` treats them as ordinary names.
fn is_safe_relative_path(path: &str) -> bool {
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

/// A symlink target may be the original relative link text, including `..`,
/// as long as resolving it from the link's parent stays inside the project.
fn symlink_target_stays_in_project(link_rel: &str, target: &str) -> bool {
    if !is_safe_relative_path(link_rel) {
        return false;
    }
    if target.is_empty() || target.contains('\\') || target.contains(':') || target.starts_with('/')
    {
        return false;
    }
    let parent = Path::new(link_rel).parent().unwrap_or(Path::new(""));
    let mut parts: Vec<&str> = parent
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
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
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return false;
                };
                parts.push(part);
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn sample() -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: "abc".into(),
            target: "//apps/web:build".into(),
            exit_code: 0,
            duration_ms: 12,
            created_at: "2026-10-13T00:00:00Z".into(),
            outputs: vec![
                OutputEntry {
                    path: "dist/app.js".into(),
                    kind: OutputKind::File,
                    digest: Some(Digest::of_bytes(b"app")),
                    mode: 0o644,
                    target: None,
                },
                OutputEntry {
                    path: "dist".into(),
                    kind: OutputKind::Dir,
                    digest: None,
                    mode: 0o755,
                    target: None,
                },
            ],
            log: Some(Digest::of_bytes(b"log")),
            aster_version: "0.17.2".into(),
            packs: Vec::new(),
            archive: None,
        }
    }

    #[test]
    fn digest_of_empty_bytes_is_known_sha256() {
        assert_eq!(Digest::of_bytes(b"").as_str(), EMPTY_SHA256);
    }

    #[test]
    fn digest_of_reader_matches_of_bytes() {
        let bytes = b"content-addressed";
        let from_reader = Digest::of_reader(&bytes[..]).unwrap();
        assert_eq!(from_reader, Digest::of_bytes(bytes));
    }

    #[test]
    fn encode_decode_round_trip() {
        let manifest = sample();
        let decoded = Manifest::decode(&manifest.encode()).expect("round trip");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn decode_rejects_other_schema() {
        let mut manifest = sample();
        manifest.schema = 2;
        assert!(Manifest::decode(&manifest.encode()).is_none());
    }

    #[test]
    fn decode_rejects_unsafe_paths() {
        for path in ["../x", "/etc/x", r"foo\..\x", "C:/windows", "C:foo", ""] {
            let mut manifest = sample();
            manifest.outputs[0].path = path.into();
            assert!(
                Manifest::decode(&manifest.encode()).is_none(),
                "accepted unsafe path {path}"
            );
        }
    }

    #[test]
    fn decode_rejects_symlink_escaping_target() {
        for target in ["/etc/passwd", "../outside", "C:foo"] {
            let mut manifest = sample();
            manifest.outputs.push(OutputEntry {
                path: "link".into(),
                kind: OutputKind::Symlink,
                digest: None,
                mode: 0o777,
                target: Some(target.into()),
            });
            assert!(
                Manifest::decode(&manifest.encode()).is_none(),
                "accepted symlink target {target}"
            );
        }

        let mut missing = sample();
        missing.outputs.push(OutputEntry {
            path: "link".into(),
            kind: OutputKind::Symlink,
            digest: None,
            mode: 0o777,
            target: None,
        });
        assert!(Manifest::decode(&missing.encode()).is_none());
    }

    #[test]
    fn decode_accepts_parent_relative_symlink_that_stays_inside() {
        let mut manifest = sample();
        manifest.outputs.push(OutputEntry {
            path: "dist/links/current".into(),
            kind: OutputKind::Symlink,
            digest: None,
            mode: 0o777,
            target: Some("../app.js".into()),
        });
        assert!(Manifest::decode(&manifest.encode()).is_some());
    }

    #[test]
    fn validate_rejects_file_without_digest() {
        let mut manifest = sample();
        manifest.outputs[0].digest = None;
        assert!(manifest.validate().is_err());
        assert!(Manifest::decode(&manifest.encode()).is_none());
    }

    #[test]
    fn blob_refs_includes_log() {
        let manifest = sample();
        let refs = manifest.blob_refs();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], manifest.outputs[0].digest.clone().unwrap());
        assert_eq!(refs[1], manifest.log.clone().unwrap());
    }

    fn archived() -> Manifest {
        let mut manifest = sample();
        let tree = std::mem::take(&mut manifest.outputs);
        manifest.outputs = vec![OutputEntry {
            path: chunk_path(0),
            kind: OutputKind::ArchiveChunk,
            digest: Some(Digest::of_bytes(b"chunk")),
            mode: 0o644,
            target: None,
        }];
        manifest.archive = Some(OutputArchive { entries: tree });
        manifest
    }

    #[test]
    fn an_archived_manifest_round_trips_and_names_only_its_chunks() {
        let manifest = archived();
        let decoded = Manifest::decode(&manifest.encode()).expect("round trip");
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.tree().len(), 2, "the tree is the archive's entries");
        assert_eq!(decoded.archive_chunks(), vec![Digest::of_bytes(b"chunk")]);
        let refs = decoded.blob_refs();
        assert_eq!(
            refs.len(),
            2,
            "one chunk and the log, not one blob per file"
        );
        assert_eq!(refs[0], Digest::of_bytes(b"chunk"));
    }

    #[test]
    fn a_client_that_predates_archives_reads_one_as_a_miss() {
        // The previous release's manifest shape: a closed set of output kinds.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum PreviousKind {
            File,
            Dir,
            Symlink,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct PreviousOutput {
            path: String,
            kind: PreviousKind,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct PreviousManifest {
            schema: u32,
            outputs: Vec<PreviousOutput>,
        }
        // Its own manifests decode, so the failure below is the new kind.
        assert!(serde_json::from_slice::<PreviousManifest>(&sample().encode()).is_ok());
        assert!(
            serde_json::from_slice::<PreviousManifest>(&archived().encode()).is_err(),
            "an older client would decode an archive's chunks as files"
        );
    }

    #[test]
    fn validate_rejects_malformed_archives() {
        let mut bad_path = archived();
        bad_path.outputs[0].path = "chunk/7".into();
        assert!(Manifest::decode(&bad_path.encode()).is_none());

        let mut no_digest = archived();
        no_digest.outputs[0].digest = None;
        assert!(Manifest::decode(&no_digest.encode()).is_none());

        let mut no_chunks = archived();
        no_chunks.outputs.clear();
        assert!(Manifest::decode(&no_chunks.encode()).is_none());

        let mut a_file_chunk = archived();
        a_file_chunk.outputs[0].kind = OutputKind::File;
        assert!(Manifest::decode(&a_file_chunk.encode()).is_none());

        let mut no_files = archived();
        no_files.archive.as_mut().unwrap().entries.remove(0);
        assert!(Manifest::decode(&no_files.encode()).is_none());

        let mut nested = archived();
        nested
            .archive
            .as_mut()
            .unwrap()
            .entries
            .push(archived().outputs.remove(0));
        assert!(Manifest::decode(&nested.encode()).is_none());

        let mut unsafe_path = archived();
        unsafe_path.archive.as_mut().unwrap().entries[0].path = "../x".into();
        assert!(Manifest::decode(&unsafe_path.encode()).is_none());
    }

    #[test]
    fn a_chunk_outside_an_archive_is_rejected() {
        let mut manifest = sample();
        manifest.outputs.push(archived().outputs.remove(0));
        assert!(Manifest::decode(&manifest.encode()).is_none());
    }
}
