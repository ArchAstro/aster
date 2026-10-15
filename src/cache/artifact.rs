//! Content-addressed artifact manifests.
//!
//! An action cache entry is a small JSON manifest keyed by a task hash.
//! File contents live in the content-addressed store as blobs keyed by
//! [`Digest`] (sha256 of the uncompressed bytes).

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
    pub(super) fn from_hex(hex: String) -> Self {
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
    /// and symlink entries whose target is missing or escapes the project.
    pub fn validate(&self) -> anyhow::Result<()> {
        for output in &self.outputs {
            if !is_safe_relative_path(&output.path) {
                anyhow::bail!("unsafe output path: {}", output.path);
            }
            match output.kind {
                OutputKind::File if output.digest.is_none() => {
                    anyhow::bail!("file output missing digest: {}", output.path);
                }
                OutputKind::Symlink => match &output.target {
                    Some(target) if is_safe_relative_path(target) => {}
                    _ => anyhow::bail!("unsafe symlink target for {}", output.path),
                },
                _ => {}
            }
        }
        Ok(())
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
}
