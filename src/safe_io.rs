//! Console output that cannot panic or stall the run.
//!
//! `eprintln!` and `println!` panic when the write fails. CI hands Aster a
//! stderr pipe that another process may have switched to `O_NONBLOCK`; once
//! the reader lags the pipe fills and the write fails with `WouldBlock`, and a
//! panic there drops everything Aster had not yet recorded.
//!
//! All console output goes through the macros here (`errln!`, `err!`,
//! `outln!`, `out!`) or the [`ConsoleErr`] / [`ConsoleOut`] writers.
//! `clippy.toml` bans the std print macros and `io::stdout()`/`io::stderr()`
//! outside this module. Behavior per stream:
//!
//! - `WouldBlock`: wait and retry. Waiting draws on one allowance shared by
//!   every writer of the stream ([`STALL_ALLOWANCE`], 30 s in total, not per
//!   write), so a slow reader receives everything while a stuck one costs at
//!   most the allowance once.
//! - Allowance spent: the stream is degraded. A write then probes the pipe for
//!   at most [`PROBE_WAIT`], at most once per [`PROBE_INTERVAL`], and drops its
//!   output otherwise. If the reader recovers, probes succeed and output
//!   resumes; if not, the stall stays bounded to a fraction of wall time.
//! - Any other error (`EPIPE`: the reader is gone): drop the output and every
//!   later write to that stream.
//! - Writers queue on the stream's lock and re-check its state after getting
//!   it, so N writers behind a stuck reader do not each wait.
//! - stderr writes mask SIGPIPE (see `without_sigpipe`); stdout keeps the
//!   default so `aster build | head` ends quietly.
//!
//! Console output is advisory. Durable artifacts (the results file, logs) must
//! never depend on it succeeding.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// Total time a stream's writers may spend waiting on a full pipe.
pub const STALL_ALLOWANCE: Duration = Duration::from_secs(30);
/// Longest one probe write waits once the allowance is spent.
pub const PROBE_WAIT: Duration = Duration::from_millis(50);
/// Fewest time between probes of a degraded stream.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_millis(20);

/// Timing knobs of a [`Stream`]; tests use small ones.
#[derive(Clone, Copy)]
pub struct Config {
    pub allowance: Duration,
    pub probe_wait: Duration,
    pub probe_interval: Duration,
}

const DEFAULT_CONFIG: Config = Config {
    allowance: STALL_ALLOWANCE,
    probe_wait: PROBE_WAIT,
    probe_interval: PROBE_INTERVAL,
};

struct State {
    /// Waiting time left; zero means degraded.
    allowance: Duration,
    last_probe: Option<Instant>,
    /// The reader is gone (a non-`WouldBlock` error): drop everything.
    dead: bool,
}

/// Health of one console stream, shared by all threads that write to it.
pub struct Stream {
    state: Mutex<State>,
    config: Config,
}

impl Stream {
    pub const fn new(config: Config) -> Self {
        Self {
            state: Mutex::new(State {
                allowance: config.allowance,
                last_probe: None,
                dead: false,
            }),
            config,
        }
    }

    /// Write `bytes` to `out`, or drop them. Never panics; the result says
    /// whether the bytes were delivered.
    pub fn write(&self, out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
        // The state lock is the queue: state is read only after getting it, so
        // a writer that waited behind a stall sees the stall's outcome.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.dead {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let probing = state.allowance.is_zero();
        let wait = if probing {
            let now = Instant::now();
            if state
                .last_probe
                .is_some_and(|at| now.duration_since(at) < self.config.probe_interval)
            {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            state.last_probe = Some(now);
            self.config.probe_wait
        } else {
            state.allowance
        };
        let started = Instant::now();
        let result = write_retrying(out, bytes, wait);
        if !probing {
            state.allowance = state.allowance.saturating_sub(started.elapsed());
        }
        if let Err(error) = &result {
            if error.kind() != io::ErrorKind::WouldBlock {
                state.dead = true;
            }
        }
        result
    }
}

static STDERR: Stream = Stream::new(DEFAULT_CONFIG);
static STDOUT: Stream = Stream::new(DEFAULT_CONFIG);

pub fn write_stderr(args: fmt::Arguments<'_>) {
    let text = args.to_string();
    let _ = stderr_bytes(text.as_bytes());
}

pub fn write_stdout(args: fmt::Arguments<'_>) {
    let text = args.to_string();
    let _ = STDOUT.write(&mut io::stdout().lock(), text.as_bytes());
}

fn stderr_bytes(bytes: &[u8]) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    without_sigpipe(|| STDERR.write(&mut stderr, bytes))
}

/// Write `bytes` to stdout with the same retry as `outln!`, but report
/// failure. For output the caller must know was delivered (JSON).
pub fn write_stdout_checked(bytes: &[u8]) -> io::Result<()> {
    STDOUT.write(&mut io::stdout().lock(), bytes)
}

/// stderr as an [`io::Write`] that never fails: undeliverable bytes are
/// dropped (see the module docs). For APIs that take a writer.
pub struct ConsoleErr;

impl Write for ConsoleErr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = stderr_bytes(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// stdout counterpart of [`ConsoleErr`].
pub struct ConsoleOut;

impl Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = write_stdout_checked(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// stderr as an indicatif draw target whose writes go through [`Stream`], so
/// progress drawing on a full non-blocking terminal neither panics nor
/// stalls past the shared allowance.
#[derive(Debug)]
pub struct ProgressTerm {
    size: console::Term,
}

impl ProgressTerm {
    pub fn draw_target() -> indicatif::ProgressDrawTarget {
        indicatif::ProgressDrawTarget::term_like_with_hz(
            Box::new(Self {
                size: console::Term::stderr(),
            }),
            20,
        )
    }

    fn put(&self, text: &str) -> io::Result<()> {
        stderr_bytes(text.as_bytes())
    }
}

impl indicatif::TermLike for ProgressTerm {
    fn width(&self) -> u16 {
        self.size.size().1
    }
    fn height(&self) -> u16 {
        self.size.size().0
    }
    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        self.put(&format!("\x1b[{n}A"))
    }
    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        self.put(&format!("\x1b[{n}B"))
    }
    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        self.put(&format!("\x1b[{n}C"))
    }
    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        self.put(&format!("\x1b[{n}D"))
    }
    fn write_line(&self, s: &str) -> io::Result<()> {
        self.put(&format!("{s}\n"))
    }
    fn write_str(&self, s: &str) -> io::Result<()> {
        self.put(s)
    }
    fn clear_line(&self) -> io::Result<()> {
        self.put("\r\x1b[2K")
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

pub fn stderr_is_terminal() -> bool {
    io::stderr().is_terminal()
}

pub fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

/// Run `write` so a write to a closed pipe returns `EPIPE` instead of
/// delivering SIGPIPE.
///
/// `main` restores the default SIGPIPE so `aster build | head` ends quietly.
/// That must not extend to stderr: a closed stderr reader would kill Aster
/// before it recorded the run's results. The mask is per thread and restored
/// before returning, so spawned targets keep the default disposition.
#[cfg(unix)]
fn without_sigpipe<T>(write: impl FnOnce() -> T) -> T {
    // SAFETY: the sigset_t values are initialized by sigemptyset before use,
    // and the mask change is per-thread and restored before returning.
    unsafe {
        let mut pipe_set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut pipe_set);
        libc::sigaddset(&mut pipe_set, libc::SIGPIPE);
        let mut old: libc::sigset_t = std::mem::zeroed();
        if libc::pthread_sigmask(libc::SIG_BLOCK, &pipe_set, &mut old) != 0 {
            return write();
        }
        let was_blocked = libc::sigismember(&old, libc::SIGPIPE) == 1;
        let result = write();
        if !was_blocked {
            // Discard a SIGPIPE raised by our write; it would otherwise be
            // delivered the moment the mask is restored.
            let mut pending: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut pending);
            if libc::sigpending(&mut pending) == 0
                && libc::sigismember(&pending, libc::SIGPIPE) == 1
            {
                let mut signal = 0;
                libc::sigwait(&pipe_set, &mut signal);
            }
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        }
        result
    }
}

#[cfg(not(unix))]
fn without_sigpipe<T>(write: impl FnOnce() -> T) -> T {
    write()
}

/// Write all of `bytes`, then flush, retrying `WouldBlock` and `Interrupted`
/// with a short growing sleep until `budget` is spent.
fn write_retrying(out: &mut dyn Write, mut bytes: &[u8], budget: Duration) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    let mut backoff = Duration::from_micros(200);
    let mut flushing = bytes.is_empty();
    loop {
        let step = if flushing {
            out.flush().map(|()| 0)
        } else {
            match out.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                other => other,
            }
        };
        match step {
            Ok(written) => {
                bytes = &bytes[written..];
                if flushing {
                    return Ok(());
                }
                if bytes.is_empty() {
                    flushing = true;
                }
                backoff = Duration::from_micros(200);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error);
                }
                thread::sleep(backoff.min(deadline - now));
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
}

/// `eprintln!` that cannot panic. See the module docs.
#[macro_export]
macro_rules! errln {
    () => { $crate::safe_io::write_stderr(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::safe_io::write_stderr(format_args!("{}\n", format_args!($($arg)*))) };
}

/// `eprint!` that cannot panic.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::safe_io::write_stderr(format_args!($($arg)*)) };
}

/// `println!` that cannot panic.
#[macro_export]
macro_rules! outln {
    () => { $crate::safe_io::write_stdout(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::safe_io::write_stdout(format_args!("{}\n", format_args!($($arg)*))) };
}

/// `print!` that cannot panic.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::safe_io::write_stdout(format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Accepts at most `chunk` bytes per call; fails the first `blocks`
    /// calls with `kind`, or every call while `stuck` is set.
    struct Flaky {
        kind: io::ErrorKind,
        blocks: usize,
        chunk: usize,
        stuck: Arc<AtomicBool>,
        got: Vec<u8>,
    }

    impl Flaky {
        fn new(kind: io::ErrorKind, blocks: usize, chunk: usize) -> Self {
            Self {
                kind,
                blocks,
                chunk,
                stuck: Arc::new(AtomicBool::new(false)),
                got: Vec::new(),
            }
        }
    }

    impl Write for Flaky {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.stuck.load(Ordering::SeqCst) {
                return Err(self.kind.into());
            }
            if self.blocks > 0 {
                self.blocks -= 1;
                return Err(self.kind.into());
            }
            let n = buf.len().min(self.chunk);
            self.got.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn quick(allowance_ms: u64) -> Stream {
        Stream::new(Config {
            allowance: Duration::from_millis(allowance_ms),
            probe_wait: Duration::from_millis(5),
            probe_interval: Duration::from_millis(40),
        })
    }

    #[test]
    fn retries_would_block_and_partial_writes_until_all_bytes_land() {
        let mut out = Flaky::new(io::ErrorKind::WouldBlock, 5, 3);
        quick(5_000).write(&mut out, b"hello world").unwrap();
        assert_eq!(out.got, b"hello world");
    }

    #[test]
    fn retries_interrupted() {
        let mut out = Flaky::new(io::ErrorKind::Interrupted, 2, 100);
        quick(5_000).write(&mut out, b"x").unwrap();
        assert_eq!(out.got, b"x");
    }

    #[test]
    fn empty_write_succeeds() {
        let mut out = Flaky::new(io::ErrorKind::WouldBlock, 0, 1);
        quick(50).write(&mut out, b"").unwrap();
    }

    #[test]
    fn other_errors_kill_the_stream_without_retry() {
        let stream = quick(5_000);
        let mut gone = Flaky::new(io::ErrorKind::BrokenPipe, 1, 1);
        let error = stream.write(&mut gone, b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        // Even a healthy writer is not tried again: the reader is gone.
        let mut healthy = Flaky::new(io::ErrorKind::WouldBlock, 0, 1);
        assert!(stream.write(&mut healthy, b"y").is_err());
        assert!(healthy.got.is_empty());
    }

    #[test]
    fn writers_queued_behind_a_stuck_reader_share_one_allowance() {
        let stream = Arc::new(quick(150));
        let started = Instant::now();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let stream = stream.clone();
                thread::spawn(move || {
                    let mut stuck = Flaky::new(io::ErrorKind::WouldBlock, usize::MAX, 1);
                    stream.write(&mut stuck, b"line\n").is_err()
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap(), "every write is dropped");
        }
        // Eight per-write budgets would be 1.2 s; one shared allowance plus
        // probes is well under that.
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "stalled {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_resumes_after_a_stall_when_the_reader_recovers() {
        let stream = quick(60);
        let mut out = Flaky::new(io::ErrorKind::WouldBlock, 0, 1024);
        out.stuck.store(true, Ordering::SeqCst);
        assert!(stream.write(&mut out, b"lost\n").is_err());
        // Degraded: the stream drops writes between probes without waiting.
        let started = Instant::now();
        for _ in 0..50 {
            let _ = stream.write(&mut out, b"lost\n");
        }
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(out.got.is_empty());

        // The reader catches up; the next probe delivers.
        out.stuck.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while stream.write(&mut out, b"back\n").is_err() {
            assert!(Instant::now() < deadline, "output never resumed");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(out.got, b"back\n");
        stream.write(&mut out, b"more\n").ok();
    }
}
