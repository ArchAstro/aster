//! What the remote cache did during one run, for the one-line summary.
//!
//! The executor builds a [`super::TieredCache`] per target, so counters live
//! in a process-wide [`RemoteStats`] that every tier records into. The run
//! ends with [`RemoteStats::take`], which returns a [`RemoteSummary`] and
//! starts the next run from zero.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Counters for one run. Cheap to share; all updates are atomic.
#[derive(Default)]
pub struct RemoteStats {
    configured: AtomicBool,
    hits: AtomicU64,
    misses: AtomicU64,
    uploads: AtomicU64,
    failures: AtomicU64,
    read_only: AtomicBool,
    disabled: Mutex<Option<String>>,
}

impl RemoteStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide instance every tier uses unless a test supplies its own.
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<RemoteStats>> = OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| Arc::new(RemoteStats::new())))
    }

    /// A tier with a remote exists, so the run gets a summary line even when
    /// nothing was looked up.
    pub(super) fn configured(&self) {
        self.configured.store(true, Ordering::Release);
    }

    pub(super) fn hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn upload(&self) {
        self.uploads.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn read_only(&self) {
        self.read_only.store(true, Ordering::Release);
    }

    /// Record why the remote stopped being used. The first reason wins.
    pub(super) fn disabled(&self, reason: &str) {
        let mut slot = self.disabled.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(reason.to_string());
        }
    }

    /// The summary so far, or `None` when no tier used a remote.
    pub fn snapshot(&self) -> Option<RemoteSummary> {
        if !self.configured.load(Ordering::Acquire) {
            return None;
        }
        Some(RemoteSummary {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            uploads: self.uploads.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            read_only: self.read_only.load(Ordering::Acquire),
            disabled: self
                .disabled
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        })
    }

    /// [`Self::snapshot`], then reset so a later run in the same process
    /// (`aster watch`) reports only its own work.
    pub fn take(&self) -> Option<RemoteSummary> {
        let summary = self.snapshot();
        self.configured.store(false, Ordering::Release);
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.uploads.store(0, Ordering::Relaxed);
        self.failures.store(0, Ordering::Relaxed);
        self.read_only.store(false, Ordering::Release);
        *self.disabled.lock().unwrap_or_else(|p| p.into_inner()) = None;
        summary
    }
}

/// One run's remote cache activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSummary {
    /// Lookups answered by the remote and restored from it.
    pub hits: u64,
    /// Lookups the remote answered with no entry.
    pub misses: u64,
    /// Manifests committed to the remote.
    pub uploads: u64,
    /// Remote calls that failed (timeouts, refused tokens, bad blobs).
    pub failures: u64,
    /// The server refused writes for this identity, so `uploads` stayed 0.
    pub read_only: bool,
    /// Why the remote was skipped for the rest of the run, if it was.
    pub disabled: Option<String>,
}

impl fmt::Display for RemoteSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "remote: {} hits / {} misses / {} uploads",
            self.hits, self.misses, self.uploads
        )?;
        if self.failures > 0 {
            write!(formatter, " / {} failures", self.failures)?;
        }
        if self.read_only {
            write!(formatter, " (read-only)")?;
        }
        if let Some(reason) = &self.disabled {
            write!(formatter, "; disabled: {reason}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_remote_means_no_summary() {
        assert_eq!(RemoteStats::new().snapshot(), None);
    }

    #[test]
    fn plain_line_has_the_three_counts() {
        let stats = RemoteStats::new();
        stats.configured();
        stats.hit();
        stats.hit();
        stats.miss();
        stats.upload();
        assert_eq!(
            stats.snapshot().unwrap().to_string(),
            "remote: 2 hits / 1 misses / 1 uploads"
        );
    }

    #[test]
    fn line_names_failures_read_only_and_the_disable_reason() {
        let stats = RemoteStats::new();
        stats.configured();
        stats.failure();
        stats.failure();
        stats.read_only();
        stats.disabled("timeout");
        stats.disabled("later reason is ignored");
        assert_eq!(
            stats.snapshot().unwrap().to_string(),
            "remote: 0 hits / 0 misses / 0 uploads / 2 failures (read-only); disabled: timeout"
        );
    }

    #[test]
    fn take_resets_for_the_next_run() {
        let stats = RemoteStats::new();
        stats.configured();
        stats.hit();
        stats.disabled("x");
        assert!(stats.take().is_some());
        assert_eq!(stats.take(), None);
        stats.configured();
        assert_eq!(
            stats.snapshot().unwrap().to_string(),
            "remote: 0 hits / 0 misses / 0 uploads"
        );
    }
}
