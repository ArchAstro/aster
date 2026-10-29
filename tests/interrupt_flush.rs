//! End-to-end: SIGINT or SIGTERM while a target runs prints that target's
//! captured output before Aster exits.
//!
//! The real `aster` binary runs a fixture target that prints, announces
//! readiness by creating a file, and then hangs. The test waits for that
//! file (an event, not a sleep), signals Aster, and checks what Aster wrote,
//! its exit status, and that the target's process is gone.
//!
//! These tests run with the rest of `tests/` under `cargo test --all-targets`
//! in CI.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Prints 20 numbered lines on stdout, then hangs. `ready` is created only
/// after the output was written, so a test that waits for it has seen the
/// output leave the target. Nothing goes to stderr: Aster reads stdout and
/// stderr on separate threads, so the order of lines across the two streams
/// is not fixed and a 15-line tail could cut either one.
const HANG_SCRIPT: &str = r#"echo $$ > pid
for i in $(seq -w 1 20); do echo "out-$i"; done
touch ready
exec sleep 1000
"#;

/// One line per stream, few enough that the tail never truncates, so both
/// show up whichever order the reader threads append them in.
const TWO_STREAMS_SCRIPT: &str = r#"echo $$ > pid
echo "out-line"
echo "err-line" >&2
touch ready
exec sleep 1000
"#;

/// Same output, but ignores SIGTERM and keeps a child, so only SIGKILL stops it.
const STUBBORN_SCRIPT: &str = r#"trap '' TERM
echo $$ > pid
echo "stubborn-output"
touch ready
while true; do sleep 1; done
"#;

fn workspace(script: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let app = tmp.path().join("app");
    fs::create_dir(&app).unwrap();
    fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(app.join("hang.sh"), script).unwrap();
    fs::write(
        app.join("aster.toml"),
        "[targets.hang]\ncommand = \"bash hang.sh\"\n",
    )
    .unwrap();
    tmp
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pid_of(tmp: &TempDir) -> i32 {
    fs::read_to_string(tmp.path().join("app/pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn process_is_running(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == -1 {
        return false;
    }
    // A zombie has exited but is not reaped yet.
    let ps = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&ps.stdout);
    !state.trim().is_empty() && !state.trim_start().starts_with('Z')
}

fn assert_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_running(pid) {
        assert!(
            Instant::now() < deadline,
            "target process {pid} still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Start `aster run //app:hang`, wait for the target to be ready, send each
/// of `signals` (in order, `gap` apart), and collect the result.
fn interrupt(tmp: &TempDir, args: &[&str], signals: &[i32], gap: Duration) -> (Output, Duration) {
    let child = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(tmp.path())
        .args(["run", "//app:hang", "--no-deps", "--no-cache"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&tmp.path().join("app/ready"));
    let mut last_signal = Instant::now();
    for (index, signal) in signals.iter().enumerate() {
        if index > 0 {
            std::thread::sleep(gap);
        }
        last_signal = Instant::now();
        unsafe {
            libc::kill(child.id() as i32, *signal);
        }
    }
    let output = child.wait_with_output().unwrap();
    (output, last_signal.elapsed())
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn sigterm_prints_running_targets_output_tail_then_exits_nonzero() {
    // Setup: a target that wrote 20 lines and is hanging.
    let tmp = workspace(HANG_SCRIPT);

    // Action: the CI runner cancels the job.
    let (output, _) = interrupt(&tmp, &[], &[libc::SIGTERM], Duration::ZERO);
    let stderr = stderr_of(&output);

    // Outcome: header plus the usual failure tail (last 15 lines), process
    // exit reflects SIGTERM, and the target's process is gone.
    assert!(
        stderr.contains("INTERRUPTED //app:hang after") && stderr.contains("output so far:"),
        "missing header:\n{stderr}"
    );
    assert!(stderr.contains("out-20"), "tail missing:\n{stderr}");
    assert!(
        !stderr.contains("out-01"),
        "output should be truncated to the tail:\n{stderr}"
    );
    assert!(stderr.contains("earlier lines omitted"), "{stderr}");
    assert_eq!(output.status.code(), Some(128 + libc::SIGTERM), "{stderr}");
    assert_gone(pid_of(&tmp));
}

#[test]
fn sigterm_report_includes_both_stdout_and_stderr_of_the_target() {
    // Setup: a target that wrote one line to each stream and is hanging.
    let tmp = workspace(TWO_STREAMS_SCRIPT);

    let (output, _) = interrupt(&tmp, &[], &[libc::SIGTERM], Duration::ZERO);
    let stderr = stderr_of(&output);

    // Outcome: the report holds both lines; their relative order is not
    // asserted because two reader threads append them.
    assert!(stderr.contains("out-line"), "stdout missing:\n{stderr}");
    assert!(
        stderr.contains("err-line"),
        "stderr stream missing:\n{stderr}"
    );
    assert_gone(pid_of(&tmp));
}

#[test]
fn sigint_with_full_logs_prints_every_captured_line() {
    let tmp = workspace(HANG_SCRIPT);

    let (output, _) = interrupt(&tmp, &["--full-logs"], &[libc::SIGINT], Duration::ZERO);
    let stderr = stderr_of(&output);

    assert!(stderr.contains("INTERRUPTED //app:hang after"), "{stderr}");
    assert!(
        stderr.contains("out-01") && stderr.contains("out-20"),
        "{stderr}"
    );
    assert!(!stderr.contains("earlier lines omitted"), "{stderr}");
    assert_eq!(output.status.code(), Some(128 + libc::SIGINT), "{stderr}");
    assert_gone(pid_of(&tmp));
}

#[test]
fn json_mode_reports_interrupted_status_and_keeps_stdout_machine_readable() {
    let tmp = workspace(HANG_SCRIPT);

    let (output, _) = interrupt(&tmp, &["--json"], &[libc::SIGTERM], Duration::ZERO);

    // stdout is exactly the JSON document; the human flush is not printed.
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {:?}", output.stdout));
    let target = &parsed["results"][0];
    assert_eq!(target["address"], "//app:hang");
    assert_eq!(target["status"], "interrupted");
    assert!(target["output"].as_str().unwrap().contains("out-20"));
    assert!(!stderr_of(&output).contains("INTERRUPTED"));
    assert_ne!(output.status.code(), Some(0));
    assert_gone(pid_of(&tmp));
}

#[test]
fn second_signal_skips_the_sigterm_grace_period() {
    // Setup: a target that ignores SIGTERM, so a lone SIGTERM waits the full
    // 3 second grace period before SIGKILL.
    let tmp = workspace(STUBBORN_SCRIPT);

    // Action: SIGTERM, then SIGTERM again shortly after.
    let (output, since_second) = interrupt(
        &tmp,
        &[],
        &[libc::SIGTERM, libc::SIGTERM],
        Duration::from_millis(300),
    );
    let stderr = stderr_of(&output);

    // Outcome: the output was still flushed, Aster exited well inside the
    // grace period, and the SIGTERM-ignoring target was killed.
    assert!(stderr.contains("INTERRUPTED //app:hang"), "{stderr}");
    assert!(stderr.contains("stubborn-output"), "{stderr}");
    assert!(
        since_second < Duration::from_millis(2500),
        "exit took {since_second:?} after the second signal"
    );
    assert_ne!(output.status.code(), Some(0));
    assert_gone(pid_of(&tmp));
}
