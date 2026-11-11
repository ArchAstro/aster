//! What the remote cache did during one run, for the one-line summary.
//!
//! The executor builds a [`super::TieredCache`] per target, so counters live
//! in a process-wide [`RemoteStats`] that every tier records into. The run
//! ends with [`RemoteStats::take`], which returns a [`RemoteSummary`] and
//! starts the next run from zero.

use serde::Serialize;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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
    io: IoCounters,
}

/// Volume and time of the remote cache's I/O, accumulated across threads.
#[derive(Default)]
struct IoCounters {
    lookup_requests: AtomicU64,
    lookup_keys: AtomicU64,
    lookup_blobs: AtomicU64,
    lookup_micros: AtomicU64,
    lookup_unresolved: AtomicU64,
    lookup_retried: AtomicU64,
    lookup_budget_misses: AtomicU64,
    lookup_timeouts: AtomicU64,
    download_blobs: AtomicU64,
    download_bytes: AtomicU64,
    download_micros: AtomicU64,
    unpacks: AtomicU64,
    unpack_micros: AtomicU64,
    upload_blobs: AtomicU64,
    upload_bytes: AtomicU64,
    upload_micros: AtomicU64,
}

impl IoCounters {
    fn snapshot(&self) -> RemoteIo {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        RemoteIo {
            lookup_requests: get(&self.lookup_requests),
            lookup_keys: get(&self.lookup_keys),
            lookup_blobs: get(&self.lookup_blobs),
            lookup_ms: get(&self.lookup_micros) / 1000,
            lookup_unresolved: get(&self.lookup_unresolved),
            lookup_retried: get(&self.lookup_retried),
            lookup_budget_misses: get(&self.lookup_budget_misses),
            lookup_timeouts: get(&self.lookup_timeouts),
            download_blobs: get(&self.download_blobs),
            download_bytes: get(&self.download_bytes),
            download_ms: get(&self.download_micros) / 1000,
            unpacks: get(&self.unpacks),
            unpack_ms: get(&self.unpack_micros) / 1000,
            upload_blobs: get(&self.upload_blobs),
            upload_bytes: get(&self.upload_bytes),
            upload_ms: get(&self.upload_micros) / 1000,
        }
    }

    fn reset(&self) {
        for counter in [
            &self.lookup_requests,
            &self.lookup_keys,
            &self.lookup_blobs,
            &self.lookup_micros,
            &self.lookup_unresolved,
            &self.lookup_retried,
            &self.lookup_budget_misses,
            &self.lookup_timeouts,
            &self.download_blobs,
            &self.download_bytes,
            &self.download_micros,
            &self.unpacks,
            &self.unpack_micros,
            &self.upload_blobs,
            &self.upload_bytes,
            &self.upload_micros,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

fn add_micros(counter: &AtomicU64, elapsed: Duration) {
    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    counter.fetch_add(micros, Ordering::Relaxed);
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

    /// One `/v1/cache/lookup` request for `keys` keys that took `elapsed`.
    /// Time spent waiting on a request that timed out counts too.
    pub(super) fn lookup_request(&self, keys: usize, elapsed: Duration) {
        let io = &self.io;
        io.lookup_requests.fetch_add(1, Ordering::Relaxed);
        io.lookup_keys.fetch_add(keys as u64, Ordering::Relaxed);
        add_micros(&io.lookup_micros, elapsed);
    }

    /// Signed blob URLs a lookup reply carried.
    pub(super) fn lookup_blobs(&self, blobs: usize) {
        self.io
            .lookup_blobs
            .fetch_add(blobs as u64, Ordering::Relaxed);
    }

    /// Keys the server left unresolved on a request: it ran out of its time
    /// budget or blob cap before it could say. Not misses yet.
    pub(super) fn lookup_unresolved(&self, keys: usize) {
        self.io
            .lookup_unresolved
            .fetch_add(keys as u64, Ordering::Relaxed);
    }

    /// Keys asked again after the server left them unresolved.
    pub(super) fn lookup_retried(&self, keys: usize) {
        self.io
            .lookup_retried
            .fetch_add(keys as u64, Ordering::Relaxed);
    }

    /// Keys still unresolved after every retry. They run as misses, but the
    /// cache may well have held them.
    pub(super) fn lookup_budget_misses(&self, keys: usize) {
        self.io
            .lookup_budget_misses
            .fetch_add(keys as u64, Ordering::Relaxed);
    }

    /// A lookup request that hit the client's timeout.
    pub(super) fn lookup_timeout(&self) {
        self.io.lookup_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// Blobs fetched from the remote into the local store.
    pub(super) fn downloaded(&self, blobs: usize, bytes: u64, elapsed: Duration) {
        let io = &self.io;
        io.download_blobs.fetch_add(blobs as u64, Ordering::Relaxed);
        io.download_bytes.fetch_add(bytes, Ordering::Relaxed);
        add_micros(&io.download_micros, elapsed);
    }

    /// One restore of a target's outputs onto disk, whatever tier held them.
    pub fn unpacked(&self, elapsed: Duration) {
        self.io.unpacks.fetch_add(1, Ordering::Relaxed);
        add_micros(&self.io.unpack_micros, elapsed);
    }

    /// Blobs sent to the remote.
    pub(super) fn uploaded(&self, blobs: usize, bytes: u64, elapsed: Duration) {
        let io = &self.io;
        io.upload_blobs.fetch_add(blobs as u64, Ordering::Relaxed);
        io.upload_bytes.fetch_add(bytes, Ordering::Relaxed);
        add_micros(&io.upload_micros, elapsed);
    }

    /// The summary of the most recent run in this process, kept after
    /// [`Self::take`] so the JSON run summary can include it.
    pub fn last_run() -> Option<RemoteSummary> {
        last_run_slot()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Remember `summary` as the last run's. `None` clears it.
    pub fn set_last_run(summary: Option<RemoteSummary>) {
        *last_run_slot().lock().unwrap_or_else(|p| p.into_inner()) = summary;
    }

    /// Whether a tier already stopped using the remote this run.
    pub(super) fn is_disabled(&self) -> bool {
        self.disabled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
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
            io: self.io.snapshot(),
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
        self.io.reset();
        summary
    }
}

fn last_run_slot() -> &'static Mutex<Option<RemoteSummary>> {
    static LAST: OnceLock<Mutex<Option<RemoteSummary>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// Remote cache I/O for one run: how many requests, how many bytes, how long.
///
/// Times are wall clock summed over calls, so concurrent calls add up to more
/// than the run took. `lookup_budget_misses` counts keys that ran as misses
/// because the server could not resolve them in time, which is different from
/// a key the cache does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemoteIo {
    pub lookup_requests: u64,
    pub lookup_keys: u64,
    pub lookup_blobs: u64,
    pub lookup_ms: u64,
    pub lookup_unresolved: u64,
    pub lookup_retried: u64,
    pub lookup_budget_misses: u64,
    pub lookup_timeouts: u64,
    pub download_blobs: u64,
    pub download_bytes: u64,
    pub download_ms: u64,
    pub unpacks: u64,
    pub unpack_ms: u64,
    pub upload_blobs: u64,
    pub upload_bytes: u64,
    pub upload_ms: u64,
}

impl fmt::Display for RemoteIo {
    /// The `remote-io:` line. Sections with no activity are left out.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "remote-io:")?;
        if self.lookup_requests > 0 {
            write!(
                formatter,
                " lookup {} requests / {} keys / {} blobs / {}",
                self.lookup_requests,
                self.lookup_keys,
                self.lookup_blobs,
                seconds(self.lookup_ms)
            )?;
            let mut notes = Vec::new();
            if self.lookup_unresolved > 0 {
                notes.push(format!(
                    "{} unresolved by the server, {} asked again",
                    self.lookup_unresolved, self.lookup_retried
                ));
            }
            if self.lookup_budget_misses > 0 {
                notes.push(format!(
                    "{} missed on the server's time budget",
                    self.lookup_budget_misses
                ));
            }
            if self.lookup_timeouts > 0 {
                notes.push(format!("{} timed out", self.lookup_timeouts));
            }
            if !notes.is_empty() {
                write!(formatter, " ({})", notes.join("; "))?;
            }
            write!(formatter, ";")?;
        }
        if self.download_blobs > 0 {
            write!(
                formatter,
                " download {} blobs / {} / {};",
                self.download_blobs,
                megabytes(self.download_bytes),
                seconds(self.download_ms)
            )?;
        }
        if self.unpacks > 0 {
            write!(
                formatter,
                " unpack {} targets / {};",
                self.unpacks,
                seconds(self.unpack_ms)
            )?;
        }
        if self.upload_blobs > 0 {
            write!(
                formatter,
                " upload {} blobs / {} / {};",
                self.upload_blobs,
                megabytes(self.upload_bytes),
                seconds(self.upload_ms)
            )?;
        }
        Ok(())
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// One run's remote cache activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// Requests, bytes and time. Printed on its own `remote-io:` line.
    pub io: RemoteIo,
}

impl RemoteSummary {
    /// The `remote-io:` line, or `None` when the run did no remote I/O.
    pub fn io_line(&self) -> Option<String> {
        (self.io != RemoteIo::default()).then(|| self.io.to_string())
    }
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
    fn io_line_names_volume_time_and_budget_misses() {
        let stats = RemoteStats::new();
        stats.configured();
        stats.lookup_request(256, Duration::from_millis(1500));
        stats.lookup_blobs(40);
        stats.lookup_request(3, Duration::from_millis(310));
        stats.lookup_blobs(9);
        stats.lookup_unresolved(3);
        stats.lookup_retried(3);
        stats.lookup_budget_misses(1);
        stats.lookup_timeout();
        stats.downloaded(5, 41_250_000, Duration::from_millis(3100));
        stats.unpacked(Duration::from_millis(200));
        stats.unpacked(Duration::from_millis(210));
        stats.uploaded(2, 1_000_000, Duration::from_millis(900));
        let summary = stats.snapshot().unwrap();
        assert_eq!(
            summary.io_line().unwrap(),
            "remote-io: lookup 2 requests / 259 keys / 49 blobs / 1.81s (3 unresolved by the \
             server, 3 asked again; 1 missed on the server's time budget; 1 timed out); \
             download 5 blobs / 41.2 MB / 3.10s; unpack 2 targets / 0.41s; \
             upload 2 blobs / 1.0 MB / 0.90s;"
        );
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["io"]["lookup_budget_misses"], 1);
        assert_eq!(json["io"]["download_bytes"], 41_250_000);
        assert_eq!(json["hits"], 0);
    }

    #[test]
    fn a_run_without_remote_io_has_no_io_line() {
        let stats = RemoteStats::new();
        stats.configured();
        assert_eq!(stats.snapshot().unwrap().io_line(), None);
    }

    #[test]
    fn take_resets_for_the_next_run() {
        let stats = RemoteStats::new();
        stats.configured();
        stats.hit();
        stats.disabled("x");
        stats.uploaded(1, 10, Duration::from_millis(5));
        assert!(stats.take().is_some());
        assert_eq!(stats.take(), None);
        stats.configured();
        let again = stats.snapshot().unwrap();
        assert_eq!(again.to_string(), "remote: 0 hits / 0 misses / 0 uploads");
        assert_eq!(again.io_line(), None);
    }
}
