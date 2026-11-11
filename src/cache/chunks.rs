//! Chunked streams and the worker pool that moves them.
//!
//! A directory cache entry and a target's output archive are both one
//! zstd-compressed stream cut into [`CHUNK_BYTES`] chunks. Each chunk is an
//! ordinary content-addressed blob, so a large entry is a few blobs rather
//! than one per file, and the chunks move in parallel.
//!
//! This module owns the two halves they share: cutting a stream into chunks
//! while workers dispose of the full ones ([`write_chunked`]), and running a
//! fixed set of indexed jobs on a bounded pool ([`run_parallel`]).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;

use super::artifact::Digest;

/// Small enough that one chunk fits the blob timeout on a slow link and a few
/// fit in memory while they wait for a worker.
pub const CHUNK_BYTES: usize = 32 * 1024 * 1024;
/// Chunks moved at once.
pub const TRANSFER_WORKERS: usize = 8;
/// Tries per chunk transfer.
pub const TRANSFER_ATTEMPTS: u32 = 3;

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `attempt` up to [`TRANSFER_ATTEMPTS`] times, waiting 1s, then 2s.
pub fn with_retries<T>(mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let mut tried = 1;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if tried >= TRANSFER_ATTEMPTS => return Err(error),
            Err(_) => {
                std::thread::sleep(Duration::from_secs(u64::from(tried)));
                tried += 1;
            }
        }
    }
}

/// Cut everything `produce` writes into [`CHUNK_BYTES`] chunks and give each
/// to `store` on a worker thread as soon as it is full.
///
/// Returns the chunk digests in stream order. `store` is retried; the first
/// chunk that still fails stops the stream and is the error returned. At most
/// [`TRANSFER_WORKERS`] chunks wait for a worker, so memory stays bounded by
/// about twice that many chunks however long the stream is.
pub fn write_chunked<S, P>(store: S, produce: P) -> Result<Vec<Digest>>
where
    S: Fn(&Digest, &[u8]) -> Result<()> + Sync,
    P: FnOnce(&mut dyn Write) -> Result<()>,
{
    let (sender, receiver) = sync_channel::<(usize, Vec<u8>)>(TRANSFER_WORKERS);
    let receiver = Mutex::new(receiver);
    let failed = AtomicBool::new(false);
    let digests = Mutex::new(Vec::<(usize, Digest)>::new());
    let first_error = Mutex::new(None::<anyhow::Error>);
    let produced = std::thread::scope(|scope| {
        for _ in 0..TRANSFER_WORKERS {
            scope.spawn(|| store_worker(&receiver, &store, &failed, &digests, &first_error));
        }
        let mut sink = ChunkSink {
            buffer: Vec::new(),
            next: 0,
            sender,
            failed: &failed,
        };
        let produced = produce(&mut sink);
        // Dropping the sink closes the channel, which ends the workers.
        produced.and_then(|()| sink.finish().map_err(Into::into))
    });
    if let Some(error) = lock(&first_error).take() {
        return Err(error);
    }
    produced?;
    let mut digests = digests
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    digests.sort_by_key(|(index, _)| *index);
    Ok(digests.into_iter().map(|(_, digest)| digest).collect())
}

fn store_worker<S>(
    receiver: &Mutex<Receiver<(usize, Vec<u8>)>>,
    store: &S,
    failed: &AtomicBool,
    digests: &Mutex<Vec<(usize, Digest)>>,
    first_error: &Mutex<Option<anyhow::Error>>,
) where
    S: Fn(&Digest, &[u8]) -> Result<()>,
{
    loop {
        // The lock is released before the work: only the wait is exclusive.
        let next = lock(receiver).recv();
        let Ok((index, bytes)) = next else {
            return;
        };
        if failed.load(Ordering::Acquire) {
            continue;
        }
        let digest = Digest::of_bytes(&bytes);
        match with_retries(|| store(&digest, &bytes)) {
            Ok(()) => lock(digests).push((index, digest)),
            Err(error) => {
                failed.store(true, Ordering::Release);
                lock(first_error).get_or_insert(error);
            }
        }
    }
}

/// Cuts a byte stream into chunks and hands them to the workers.
struct ChunkSink<'a> {
    buffer: Vec<u8>,
    next: usize,
    sender: SyncSender<(usize, Vec<u8>)>,
    failed: &'a AtomicBool,
}

impl ChunkSink<'_> {
    fn send(&mut self) -> io::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("a chunk transfer failed"));
        }
        let chunk = std::mem::take(&mut self.buffer);
        self.sender
            .send((self.next, chunk))
            .map_err(|_| io::Error::other("the chunk workers stopped"))?;
        self.next += 1;
        Ok(())
    }

    /// Send the last, partial chunk.
    fn finish(mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.shrink_to_fit();
        self.send()
    }
}

impl Write for ChunkSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.buffer.capacity() == 0 {
            // A stream that ends early gives the memory back at once.
            self.buffer.reserve_exact(CHUNK_BYTES);
        }
        let room = CHUNK_BYTES - self.buffer.len();
        let taken = room.min(bytes.len());
        self.buffer.extend_from_slice(&bytes[..taken]);
        if self.buffer.len() == CHUNK_BYTES {
            self.send()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run `work(index)` for every index below `count` that `skip` does not
/// exclude, on up to [`TRANSFER_WORKERS`] threads.
///
/// Stops handing out indexes after the first failure and returns that
/// failure once the running jobs finish.
pub fn run_parallel<W, K>(count: usize, skip: K, work: W) -> Result<()>
where
    W: Fn(usize) -> Result<()> + Sync,
    K: Fn(usize) -> bool + Sync,
{
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None::<anyhow::Error>);
    std::thread::scope(|scope| {
        for _ in 0..TRANSFER_WORKERS.min(count) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= count || failed.load(Ordering::Acquire) {
                    return;
                }
                if skip(index) {
                    continue;
                }
                if let Err(error) = work(index) {
                    failed.store(true, Ordering::Release);
                    lock(&first_error).get_or_insert(error);
                }
            });
        }
    });
    let error = lock(&first_error).take();
    error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn incompressible(length: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut bytes = Vec::with_capacity(length + 8);
        while bytes.len() < length {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(length);
        bytes
    }

    #[test]
    fn a_stream_is_cut_at_the_chunk_size_and_digests_keep_stream_order() {
        let stream = incompressible(CHUNK_BYTES * 2 + 123);
        let stored = Mutex::new(HashMap::<String, Vec<u8>>::new());
        let digests = write_chunked(
            |digest, bytes| {
                lock(&stored).insert(digest.as_str().to_string(), bytes.to_vec());
                Ok(())
            },
            |sink| {
                // Uneven writes, so chunk edges fall inside a write.
                for piece in stream.chunks(1_000_003) {
                    sink.write_all(piece)?;
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(digests.len(), 3);
        let stored = stored.into_inner().unwrap();
        let rejoined: Vec<u8> = digests
            .iter()
            .flat_map(|digest| stored[digest.as_str()].clone())
            .collect();
        assert_eq!(rejoined, stream);
        assert_eq!(stored[digests[0].as_str()].len(), CHUNK_BYTES);
        assert_eq!(stored[digests[2].as_str()].len(), 123);
    }

    #[test]
    fn an_empty_stream_has_no_chunks() {
        let digests = write_chunked(|_, _| Ok(()), |_| Ok(())).unwrap();
        assert!(digests.is_empty());
    }

    #[test]
    fn a_failing_store_is_the_error_and_stops_the_stream() {
        let attempts = AtomicUsize::new(0);
        let result = write_chunked(
            |_, _| {
                attempts.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("disk full")
            },
            |sink| {
                // Far more than the workers can hold: the producer must be
                // stopped by the failure rather than write all of it.
                let block = vec![7_u8; CHUNK_BYTES];
                for _ in 0..64 {
                    sink.write_all(&block)?;
                }
                Ok(())
            },
        );
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("disk full"), "{error:#}");
        assert!(attempts.load(Ordering::SeqCst) >= TRANSFER_ATTEMPTS as usize);
    }

    #[test]
    fn a_producer_error_is_returned() {
        let result = write_chunked(|_, _| Ok(()), |_| anyhow::bail!("walk failed"));
        assert!(format!("{:#}", result.unwrap_err()).contains("walk failed"));
    }

    #[test]
    fn run_parallel_runs_each_index_once_and_skips_the_excluded() {
        let seen = Mutex::new(Vec::new());
        run_parallel(
            50,
            |index| index % 5 == 0,
            |index| {
                lock(&seen).push(index);
                Ok(())
            },
        )
        .unwrap();
        let mut seen = seen.into_inner().unwrap();
        seen.sort_unstable();
        let expected: Vec<usize> = (0..50).filter(|index| index % 5 != 0).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn run_parallel_returns_the_first_failure() {
        let result = run_parallel(
            100,
            |_| false,
            |index| {
                if index == 3 {
                    anyhow::bail!("job 3 broke");
                }
                Ok(())
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("job 3 broke"));
    }
}
