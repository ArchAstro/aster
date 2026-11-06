//! A failure dump into a stderr pipe that cannot keep up must not lose the
//! run's results or change its exit code.
//!
//! CI gave Aster a stderr pipe that another process had switched to
//! `O_NONBLOCK`. Once the reader lagged, `eprintln!` hit `EAGAIN` while
//! printing a large failed target's output and panicked (exit 101) before the
//! test results file was written. These tests run the real `aster` binary
//! with that pipe, and with a pipe whose reader has gone away.
//!
//! They run with the rest of `tests/` under `cargo test --all-targets`.
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use aster_results_model::{parse_ndjson, Record, TargetStatus};
use tempfile::TempDir;

/// Lines the target prints. About 1.3 MB, far above any pipe buffer.
const LARGE: u32 = 200_000;
const SMALL: u32 = 20;

fn workspace(lines: u32, exit_code: i32) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let app = tmp.path().join("app");
    fs::create_dir(&app).unwrap();
    fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        app.join("aster.toml"),
        format!("[targets.noisy]\ncommand = \"sh -c 'seq 1 {lines}; exit {exit_code}'\"\n"),
    )
    .unwrap();
    tmp
}

#[derive(Clone, Copy, PartialEq)]
enum Pipe {
    Blocking,
    NonBlocking,
}

/// A pipe as `(read, write)`; the write end is non-blocking on request.
fn pipe(mode: Pipe) -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    if mode == Pipe::NonBlocking {
        unsafe {
            let flags = libc::fcntl(fds[1], libc::F_GETFL);
            assert_ne!(
                libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK),
                -1
            );
        }
    }
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

struct Outcome {
    code: Option<i32>,
    /// Everything the reader received; `None` when the reader hung up early.
    stderr: Option<String>,
    results: Vec<Record>,
}

/// Run `aster run //app:noisy --full-logs` with stderr on a pipe.
/// `reader`: `Some(delay)` reads everything after sleeping `delay`; `None`
/// closes the read end before Aster starts.
fn run(tmp: &TempDir, mode: Pipe, reader: Option<Duration>) -> Outcome {
    let results_path = tmp.path().join("results.ndjson");
    let (read, write) = pipe(mode);
    let mut read_end = Some(read);
    let reading = reader.map(|delay| {
        let mut file = fs::File::from(read_end.take().unwrap());
        thread::spawn(move || {
            thread::sleep(delay);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });
    drop(read_end);

    let mut child = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .env_remove("ASTER_TEST_RESULTS")
        .env_remove("GITHUB_RUN_ID")
        .env("RUNNER_TEMP", tmp.path().join("runner-temp"))
        .arg("--no-cache")
        .arg(format!("--test-results={}", results_path.display()))
        .args(["--full-logs", "noisy", "//app"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(write))
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();

    let results = fs::read_to_string(&results_path)
        .map(|text| parse_ndjson(&text).unwrap())
        .unwrap_or_default();
    Outcome {
        code: status.code(),
        stderr: reading.map(|handle| handle.join().unwrap()),
        results,
    }
}

fn target_record(results: &[Record]) -> (&TargetStatus, Option<i32>) {
    results
        .iter()
        .find_map(|record| match record {
            Record::Target(target) if target.address == "//app:noisy" => {
                Some((&target.status, target.exit_code))
            }
            _ => None,
        })
        .expect("the results file has a record for //app:noisy")
}

/// Closed stderr reader: no panic (101), no SIGPIPE death, the target's
/// failure is the exit status, and the results file is complete.
fn assert_closed_reader_outcome(outcome: &Outcome) {
    assert_eq!(outcome.code, Some(1), "101 would be the panic");
    assert_eq!(
        target_record(&outcome.results),
        (&TargetStatus::Failed, Some(3))
    );
}

fn assert_full_dump(stderr: &str, last: u32) {
    assert!(
        stderr.contains(&format!("    {last}\n")),
        "the last output line reached the reader (got {} bytes)",
        stderr.len()
    );
    assert!(stderr.contains("FAILED //app:noisy"));
}

/// Canonical proof: a 1.3 MB failure dump into a full non-blocking stderr
/// pipe with a reader that starts four seconds late.
#[test]
fn large_failure_dump_into_nonblocking_stderr_keeps_exit_code_results_and_output() {
    // Setup: a target that fails after printing far more than a pipe holds.
    let tmp = workspace(LARGE, 3);

    // Action: stderr is O_NONBLOCK and nobody reads for a while, so the dump
    // hits EAGAIN until the reader wakes.
    let outcome = run(&tmp, Pipe::NonBlocking, Some(Duration::from_secs(4)));

    // Outcome: Aster reports the run failure (1), not a panic (101) ...
    assert_eq!(outcome.code, Some(1), "stderr tail: {:?}", outcome.stderr);
    // ... the results file was written and names the target's real exit ...
    assert_eq!(
        target_record(&outcome.results),
        (&TargetStatus::Failed, Some(3))
    );
    // ... and the reader still got the whole dump.
    assert_full_dump(outcome.stderr.as_deref().unwrap(), LARGE);
}

#[test]
fn large_failure_dump_into_blocking_stderr_is_delivered() {
    let tmp = workspace(LARGE, 3);
    let outcome = run(&tmp, Pipe::Blocking, Some(Duration::from_secs(1)));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(
        target_record(&outcome.results),
        (&TargetStatus::Failed, Some(3))
    );
    assert_full_dump(outcome.stderr.as_deref().unwrap(), LARGE);
}

#[test]
fn closed_stderr_pipe_does_not_panic_or_lose_results() {
    let tmp = workspace(LARGE, 3);
    let outcome = run(&tmp, Pipe::Blocking, None);
    assert_closed_reader_outcome(&outcome);
}

#[test]
fn closed_nonblocking_stderr_pipe_does_not_panic_or_lose_results() {
    let tmp = workspace(LARGE, 3);
    let outcome = run(&tmp, Pipe::NonBlocking, None);
    assert_closed_reader_outcome(&outcome);
}

#[test]
fn small_failure_into_nonblocking_stderr_is_delivered() {
    let tmp = workspace(SMALL, 3);
    let outcome = run(&tmp, Pipe::NonBlocking, Some(Duration::from_millis(200)));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(
        target_record(&outcome.results),
        (&TargetStatus::Failed, Some(3))
    );
    assert_full_dump(outcome.stderr.as_deref().unwrap(), SMALL);
}

#[test]
fn passing_large_target_into_nonblocking_stderr_succeeds() {
    let tmp = workspace(LARGE, 0);
    let outcome = run(&tmp, Pipe::NonBlocking, Some(Duration::from_secs(1)));
    assert_eq!(outcome.code, Some(0), "stderr: {:?}", outcome.stderr);
    assert_eq!(
        target_record(&outcome.results),
        (&TargetStatus::Passed, Some(0))
    );
}
