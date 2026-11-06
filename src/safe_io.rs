//! Console output that cannot panic.
//!
//! `eprintln!` and `println!` panic when the write fails. CI hands Aster a
//! stderr pipe that another process may have switched to `O_NONBLOCK`; once
//! the reader lags the pipe fills and the write fails with `WouldBlock`, and a
//! panic there drops everything Aster had not yet recorded.
//!
//! This module defines `eprintln!`, `eprint!`, `println!` and `print!` with
//! the same syntax as the std macros. Declaring it first in the crate
//! (`#[macro_use]`) makes every call in the crate use these instead:
//!
//! - `WouldBlock`: wait briefly and retry, for up to [`WOULD_BLOCK_BUDGET`]
//!   per write, so a slow reader still receives the full output.
//! - Any other error (`EPIPE` from a closed reader, a stuck reader after the
//!   budget): drop the output. The stream is then treated as dead and later
//!   writes return immediately, so a gone reader costs at most one budget.
//!
//! Console output is advisory. Durable artifacts (the results file, logs) must
//! never depend on it succeeding.

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Longest a single write waits for a full non-blocking pipe to drain.
const WOULD_BLOCK_BUDGET: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_millis(20);

static STDERR_DEAD: AtomicBool = AtomicBool::new(false);
static STDOUT_DEAD: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub fn write_stderr(args: fmt::Arguments<'_>) {
    if STDERR_DEAD.load(Ordering::Relaxed) {
        return;
    }
    let text = args.to_string();
    let mut stderr = io::stderr().lock();
    let written =
        without_sigpipe(|| write_retrying(&mut stderr, text.as_bytes(), WOULD_BLOCK_BUDGET));
    if written.is_err() {
        STDERR_DEAD.store(true, Ordering::Relaxed);
    }
}

/// Run `write` so a write to a closed pipe returns `EPIPE` instead of
/// delivering SIGPIPE.
///
/// `main` restores the default SIGPIPE so `aster build | head` ends quietly.
/// That must not extend to stderr: a closed stderr reader would kill Aster
/// before it recorded the run's results. Stdout keeps the default.
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

#[allow(dead_code)]
pub fn write_stdout(args: fmt::Arguments<'_>) {
    if STDOUT_DEAD.load(Ordering::Relaxed) {
        return;
    }
    let text = args.to_string();
    let mut stdout = io::stdout().lock();
    if write_retrying(&mut stdout, text.as_bytes(), WOULD_BLOCK_BUDGET).is_err() {
        STDOUT_DEAD.store(true, Ordering::Relaxed);
    }
}

/// Write `bytes` to stdout with the same `WouldBlock` retry as `println!`,
/// but report failure. For output the caller must know was delivered (JSON).
#[allow(dead_code)]
pub fn write_stdout_checked(bytes: &[u8]) -> io::Result<()> {
    write_retrying(&mut io::stdout().lock(), bytes, WOULD_BLOCK_BUDGET)
}

/// Write all of `bytes`, then flush, retrying `WouldBlock` and `Interrupted`
/// with a short growing sleep until `budget` is spent.
fn write_retrying(out: &mut impl Write, mut bytes: &[u8], budget: Duration) -> io::Result<()> {
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
                if Instant::now() >= deadline {
                    return Err(error);
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(unused_macros)]
macro_rules! eprintln {
    () => { $crate::safe_io::write_stderr(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::safe_io::write_stderr(format_args!("{}\n", format_args!($($arg)*))) };
}

#[allow(unused_macros)]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::safe_io::write_stderr(format_args!($($arg)*)) };
}

#[allow(unused_macros)]
macro_rules! println {
    () => { $crate::safe_io::write_stdout(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::safe_io::write_stdout(format_args!("{}\n", format_args!($($arg)*))) };
}

#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => { $crate::safe_io::write_stdout(format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts at most `chunk` bytes per call and fails the first `blocks`
    /// calls with the given error kind.
    struct Flaky {
        kind: io::ErrorKind,
        blocks: usize,
        chunk: usize,
        got: Vec<u8>,
    }

    impl Write for Flaky {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
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

    #[test]
    fn retries_would_block_and_partial_writes_until_all_bytes_land() {
        let mut out = Flaky {
            kind: io::ErrorKind::WouldBlock,
            blocks: 5,
            chunk: 3,
            got: Vec::new(),
        };
        write_retrying(&mut out, b"hello world", Duration::from_secs(5)).unwrap();
        assert_eq!(out.got, b"hello world");
    }

    #[test]
    fn retries_interrupted() {
        let mut out = Flaky {
            kind: io::ErrorKind::Interrupted,
            blocks: 2,
            chunk: 100,
            got: Vec::new(),
        };
        write_retrying(&mut out, b"x", Duration::from_secs(5)).unwrap();
        assert_eq!(out.got, b"x");
    }

    #[test]
    fn gives_up_on_a_reader_that_never_drains() {
        let mut out = Flaky {
            kind: io::ErrorKind::WouldBlock,
            blocks: usize::MAX,
            chunk: 1,
            got: Vec::new(),
        };
        let error = write_retrying(&mut out, b"x", Duration::from_millis(30)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn other_errors_are_returned_without_retry() {
        let mut out = Flaky {
            kind: io::ErrorKind::BrokenPipe,
            blocks: 1,
            chunk: 1,
            got: Vec::new(),
        };
        let error = write_retrying(&mut out, b"x", Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(out.got.is_empty());
    }
}
