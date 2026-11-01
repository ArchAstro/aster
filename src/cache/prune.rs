//! Limits and the `aster cache prune` entry point.
//!
//! Sizes accept a bare byte count or a `KB`/`MB`/`GB` suffix (powers of 1024).
//! Ages accept `s`/`m`/`h`/`d`. `0` disables that limit.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Duration;

use super::local::{LocalStore, PruneStats};
use super::resolve_cache_dir;

/// Human summary of one prune. Auto-prune prefixes this with `[aster] `.
pub fn format_prune_summary(stats: &PruneStats) -> String {
    format!(
        "Pruned {} manifests, {} blobs, and {} temp files ({} bytes remaining)",
        stats.manifests_removed,
        stats.blobs_removed,
        stats.tmp_files_removed,
        stats.bytes_remaining
    )
}

/// Default cap used when neither the flag nor workspace config sets a size.
pub const DEFAULT_MAX_SIZE: u64 = 10 * 1024 * 1024 * 1024;
/// Default age used when neither the flag nor workspace config sets an age.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Resolved prune limits. `None` means that limit is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneLimits {
    pub max_size: Option<u64>,
    pub max_age: Option<Duration>,
}

impl PruneLimits {
    /// Defaults: 10 GB and 14 days.
    pub fn defaults() -> Self {
        Self {
            max_size: Some(DEFAULT_MAX_SIZE),
            max_age: Some(DEFAULT_MAX_AGE),
        }
    }

    /// Workspace config, with defaults for fields that are absent. `0` disables.
    pub fn from_config(max_size: Option<&str>, max_age: Option<&str>) -> Result<Self> {
        let mut limits = Self::defaults();
        if let Some(raw) = max_size {
            limits.max_size = parse_size(raw).context("invalid cache max_size")?;
        }
        if let Some(raw) = max_age {
            limits.max_age = parse_age(raw).context("invalid cache max_age")?;
        }
        Ok(limits)
    }

    /// CLI flags override an already-resolved config. Absent flags keep it.
    pub fn with_flags(mut self, max_size: Option<&str>, max_age: Option<&str>) -> Result<Self> {
        if let Some(raw) = max_size {
            self.max_size = parse_size(raw).context("invalid --max-size")?;
        }
        if let Some(raw) = max_age {
            self.max_age = parse_age(raw).context("invalid --max-age")?;
        }
        Ok(self)
    }
}

/// Parse a byte size. `None` means the limit is disabled (`0`).
pub fn parse_size(raw: &str) -> Result<Option<u64>> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("size is empty");
    }
    let (number, factor) = split_suffix(
        text,
        &[
            ("gb", 1024u64.pow(3)),
            ("mb", 1024u64.pow(2)),
            ("kb", 1024),
            ("b", 1),
        ],
    )?;
    let value = parse_u64(number).with_context(|| format!("invalid size '{raw}'"))?;
    if value == 0 {
        return Ok(None);
    }
    value
        .checked_mul(factor)
        .map(Some)
        .with_context(|| format!("size '{raw}' overflows"))
}

/// Parse an age. `None` means the limit is disabled (`0`).
pub fn parse_age(raw: &str) -> Result<Option<Duration>> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("age is empty");
    }
    let (number, factor) = split_suffix(text, &[("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)])?;
    let value = parse_u64(number).with_context(|| format!("invalid age '{raw}'"))?;
    if value == 0 {
        return Ok(None);
    }
    let seconds = value
        .checked_mul(factor)
        .with_context(|| format!("age '{raw}' overflows"))?;
    Ok(Some(Duration::from_secs(seconds)))
}

fn split_suffix<'a>(text: &'a str, suffixes: &[(&str, u64)]) -> Result<(&'a str, u64)> {
    let lower = text.to_ascii_lowercase();
    for (suffix, factor) in suffixes {
        if let Some(number) = lower.strip_suffix(suffix) {
            let number = number.trim();
            if number.is_empty() {
                bail!("missing number before '{suffix}' in '{text}'");
            }
            let split_at = text.len() - suffix.len();
            return Ok((text[..split_at].trim(), *factor));
        }
    }
    Ok((text, 1))
}

fn parse_u64(number: &str) -> Result<u64> {
    let number = number.trim();
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("expected an integer, got '{number}'");
    }
    number.parse::<u64>().context("integer overflow")
}

/// Prune the workspace cache and return what was removed.
pub fn prune_cache(workspace_root: &Path, limits: PruneLimits) -> Result<PruneStats> {
    let store = LocalStore::new(resolve_cache_dir(workspace_root));
    store.prune(limits.max_size, limits.max_age)
}

/// Prune after a run when the store is over its size cap. Age is applied in
/// the same pass. A store at or under the cap, or with size disabled, is left
/// for an explicit `aster cache prune`. A failure is returned to the caller,
/// which treats it as a warning so a build is not failed by eviction.
pub fn prune_if_over_cap(workspace_root: &Path, limits: PruneLimits) -> Result<Option<PruneStats>> {
    let Some(max_size) = limits.max_size else {
        return Ok(None);
    };
    let store = LocalStore::new(resolve_cache_dir(workspace_root));
    if store.cas_bytes()? <= max_size {
        return Ok(None);
    }
    let stats = store.prune(Some(max_size), limits.max_age)?;
    if stats.manifests_removed == 0 && stats.blobs_removed == 0 && stats.tmp_files_removed == 0 {
        return Ok(None);
    }
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::artifact::{
        Digest, Manifest, OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION,
    };
    use std::time::SystemTime;

    #[test]
    fn parse_size_and_age_accept_suffixes() {
        assert_eq!(parse_size("10GB").unwrap(), Some(10 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1MB").unwrap(), Some(1024 * 1024));
        assert_eq!(parse_size("2KB").unwrap(), Some(2048));
        assert_eq!(parse_size("512").unwrap(), Some(512));
        assert_eq!(parse_size("0").unwrap(), None);
        assert_eq!(parse_size("0GB").unwrap(), None);
        assert!(parse_size("10TB").is_err());
        assert!(parse_size("").is_err());

        assert_eq!(
            parse_age("14d").unwrap(),
            Some(Duration::from_secs(14 * 86_400))
        );
        assert_eq!(parse_age("2h").unwrap(), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_age("30m").unwrap(), Some(Duration::from_secs(1_800)));
        assert_eq!(parse_age("45s").unwrap(), Some(Duration::from_secs(45)));
        assert_eq!(parse_age("0").unwrap(), None);
        assert!(parse_age("14w").is_err());

        let limits = PruneLimits::defaults()
            .with_flags(Some("1KB"), Some("0"))
            .unwrap();
        assert_eq!(limits.max_size, Some(1024));
        assert_eq!(limits.max_age, None);

        let summary = format_prune_summary(&PruneStats {
            manifests_removed: 1,
            blobs_removed: 2,
            tmp_files_removed: 3,
            bytes_remaining: 4,
        });
        assert_eq!(
            summary,
            "Pruned 1 manifests, 2 blobs, and 3 temp files (4 bytes remaining)"
        );
    }

    fn sample(key: &str, digest: &Digest) -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: key.into(),
            target: "//apps/web:build".into(),
            exit_code: 0,
            duration_ms: 1,
            created_at: "2026-10-13T00:00:00Z".into(),
            outputs: vec![OutputEntry {
                path: "dist/app.bin".into(),
                kind: OutputKind::File,
                digest: Some(digest.clone()),
                mode: 0o644,
                target: None,
            }],
            log: None,
            aster_version: "0.17.2".into(),
            packs: Vec::new(),
        }
    }

    #[test]
    fn prune_command_uses_limits_and_reports_removed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let cache_dir = workspace.join(".aster").join("cache");
        let store = LocalStore::new(cache_dir);
        let old = Digest::of_bytes(b"old-command-blob");
        let new = Digest::of_bytes(b"new-command-blob");
        store.put_blob(&old, &b"old-command-blob"[..]).unwrap();
        store.put_blob(&new, &b"new-command-blob"[..]).unwrap();
        let old_key = "aa1111111111111111";
        let new_key = "bb2222222222222222";
        store.put_manifest(&sample(old_key, &old)).unwrap();
        store.put_manifest(&sample(new_key, &new)).unwrap();

        let old_path = store.manifest_path(old_key).unwrap();
        let new_path = store.manifest_path(new_key).unwrap();
        set_mtime(&old_path, SystemTime::UNIX_EPOCH + Duration::from_secs(10));
        set_mtime(&new_path, SystemTime::UNIX_EPOCH + Duration::from_secs(20));
        // Past prune's grace window for unreferenced blobs.
        set_mtime(
            &store.blob_path(&old).unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        );
        let new_bytes = std::fs::metadata(store.blob_path(&new).unwrap())
            .unwrap()
            .len();

        let limits = PruneLimits::defaults()
            .with_flags(Some(&new_bytes.to_string()), Some("0"))
            .unwrap();
        let stats = prune_cache(workspace, limits).unwrap();
        assert_eq!(stats.manifests_removed, 1);
        assert_eq!(stats.blobs_removed, 1);
        assert!(!store.has_blob(&old));
        assert!(store.has_blob(&new));
        assert!(store.get_manifest(new_key).unwrap().is_some());
    }

    fn set_mtime(path: &std::path::Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }
}
