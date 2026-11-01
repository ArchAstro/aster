//! Real CLI/process proofs of host admission. Each test has its own host state;
//! no test acquires a developer's execution slots or signals unrelated jobs.
//!
//! Canonical proof: concurrent_workspaces_share_capacity_and_release_after_child_exit.
//! Integration matrix: run/affected/streaming; one/multiple ready targets; one/two
//! workspaces and invocations; equal/different host keys; same/different workspace
//! keys; cold/warm cache; normal/failure/waiting cancellation/scheduler SIGKILL;
//! every admitted group SIGKILLed (stale family cleanup); recursive nested siblings;
//! workspace/state symlink aliases; inherited/new resources; captured/streaming
//! first/second interruption of stubborn children; conflicting `--jobs` in
//! human/quiet/json, plus a same-jobs control.
//! Fidelity: production CLI, scheduler, flock, process groups, Git affected
//! selection, and socket barriers. Python commands replace expensive compilers,
//! not the admission or lifecycle code. The test closes their inherited FDs.
//! Automatic coverage: .github/workflows/ci.yml on PRs and main pushes, Linux
//! --workspace --all-targets and macOS's explicit --test execution_capacity.
//! Not proven here: Windows (feature disabled), old binaries (no protocol),
//! arbitrary detached processes/private-supervisor SIGKILL, or compiler thread
//! counts/firstlanding throughput (outside the admitted-command-family contract).
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BUDGET: Duration = Duration::from_secs(15);

struct Run(Child, std::sync::mpsc::Receiver<String>);
impl Run {
    fn spawn(mut command: Command) -> Self {
        let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[child] {line}");
                let _ = tx.send(line);
            }
        });
        Self(child, rx)
    }
    fn waiting(&self, listener: &TcpListener) {
        let deadline = Instant::now() + BUDGET;
        loop {
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "contender started before admission was released"
            );
            match self.1.try_recv() {
                Ok(line) if line.contains("Waiting for execution admission:") => return,
                Ok(_) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(error) => panic!("contender exited before reaching admission: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "contender did not reach admission"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn finish(&mut self) -> bool {
        let deadline = Instant::now() + BUDGET;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status.success();
            }
            assert!(Instant::now() < deadline, "Aster did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(self.0.id() as i32, signal);
        }
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Target {
    stream: TcpStream,
    label: String,
}
impl Target {
    fn release(mut self) {
        self.stream.write_all(b"finish\n").unwrap();
    }
}

struct Fixture {
    root: TempDir,
    listener: TcpListener,
    binary: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::write(
            root.path().join("probe.py"),
            r#"
import os, signal, socket, sys
# The production supervisor must own the leases even when a tool closes FDs.
os.closerange(3, 256)
if sys.argv[3] == 'descendant':
    if os.fork():
        os._exit(0)
s = socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=15)
if sys.argv[3] == 'ignoreterm':
    signal.signal(signal.SIGTERM, lambda *_: s.sendall(b'term\n'))
s.sendall((sys.argv[2] + '\n').encode())
s.recv(32)  # Test-owned socket closure also releases the child on assertion failure.
s.close()
sys.exit(7 if sys.argv[3] == 'fail' else 0)
"#,
        )
        .unwrap();
        let binary = std::env::var_os("ASTER_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(assert_cmd::cargo::cargo_bin!("aster")));
        Self {
            root,
            listener,
            binary,
        }
    }
    fn workspace(&self, name: &str, resource: &str, mode: &str) -> PathBuf {
        let root = self.root.path().join(name);
        fs::create_dir(&root).unwrap();
        fs::write(root.join("package.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        let command = format!(
            "python3 {} {} {name} {mode}",
            aster::executor::quote_command_argument(
                &self.root.path().join("probe.py").display().to_string()
            ),
            self.listener.local_addr().unwrap().port()
        );
        let resource = if resource.is_empty() {
            "[]".into()
        } else {
            format!("[{resource:?}]")
        };
        fs::write(root.join("aster.toml"), format!("name = '{name}'\n[targets.work]\ncommand = {command:?}\ncache = {{ enabled = false }}\nexclusive_resources = {resource}\n")).unwrap();
        root
    }
    fn command(&self, root: &Path, jobs: usize) -> Command {
        let mut c = Command::new(&self.binary);
        c.current_dir(root)
            .env("ASTER_EXECUTION_DIR", self.root.path().join("host"))
            .env("ASTER_JOBS", jobs.to_string())
            .env_remove("ASTER_EXECUTION_CONTEXT")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        c
    }
    fn start(&self, root: &Path, jobs: usize, target: &str) -> Run {
        let mut command = self.command(root, jobs);
        command.args(["run", target, "--quiet"]);
        Run::spawn(command)
    }
    fn stream(&self, root: &Path, jobs: usize, target: &str) -> Run {
        let mut command = self.command(root, jobs);
        command.args([target, ".", "--stream", "--quiet"]);
        Run::spawn(command)
    }
    fn started(&self) -> Target {
        let deadline = Instant::now() + BUDGET;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(BUDGET)).unwrap();
                    let mut label = String::new();
                    BufReader::new(&stream).read_line(&mut label).unwrap();
                    return Target {
                        stream,
                        label: label.trim().into(),
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "target never started");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
    }
    fn remains_blocked(&self) {
        // The first real child remains behind its explicit socket barrier.
        // Bound only this negative observation, not production ownership.
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            assert!(
                matches!(self.listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "conflicting target started while its owner was still live"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn concurrent_workspaces_share_capacity_and_release_after_child_exit() {
    let f = Fixture::new();
    let a = f.workspace("a", "", "normal");
    let b = f.workspace("b", "", "normal");
    let mut first = f.start(&a, 1, "//:work");
    let held = f.started();
    assert_eq!(held.label, "a");
    let mut next = f.start(&b, 1, "//:work");
    next.waiting(&f.listener);
    f.remains_blocked();
    // Fault: kill the scheduler, not its admitted child or lease owner.
    first.signal(libc::SIGKILL);
    assert!(!first.finish());
    f.remains_blocked();
    held.release();
    let started = f.started();
    assert_eq!(started.label, "b");
    started.release();
    assert!(next.finish());
}

#[test]
fn named_host_resources_serialize_but_workspace_resources_are_independent() {
    for (resource, other, same_workspace, serial) in [
        ("host:postgres", "host:postgres", false, true),
        ("host:one", "host:two", false, false),
        ("workspace:compiler", "workspace:compiler", true, true),
        ("workspace:compiler", "workspace:compiler", false, false),
        ("legacy-local", "legacy-local", false, false),
    ] {
        let f = Fixture::new();
        let a = f.workspace("a", resource, "normal");
        let b = if same_workspace {
            a.clone()
        } else {
            f.workspace("b", other, "normal")
        };
        let mut first = f.start(&a, 2, "//:work");
        let held = f.started();
        let mut next = f.start(&b, 2, "//:work");
        if serial {
            next.waiting(&f.listener);
            f.remains_blocked();
            held.release();
            f.started().release();
        } else {
            let concurrent = f.started();
            assert_ne!(held.label, concurrent.label);
            held.release();
            concurrent.release();
        }
        assert!(first.finish());
        assert!(next.finish());
    }
}

#[test]
fn leader_exit_does_not_release_capacity_held_by_its_remaining_group() {
    let f = Fixture::new();
    let a = f.workspace("a", "host:database", "descendant");
    let b = f.workspace("b", "host:database", "fail");
    let mut first = f.start(&a, 1, "//:work");
    let descendant = f.started();
    let mut next = f.start(&b, 1, "//:work");
    next.waiting(&f.listener);
    f.remains_blocked();
    descendant.release();
    f.started().release();
    assert!(first.finish());
    assert!(!next.finish());
    // Failure also returns the permit to another real invocation.
    let mut after = f.start(&a, 1, "//:work");
    f.started().release();
    assert!(after.finish());
}

#[test]
fn active_capacity_is_authoritative_and_can_change_when_idle() {
    let f = Fixture::new();
    let a = f.workspace("a", "", "normal");
    let mut first = f.start(&a, 1, "//:work");
    let held = f.started();
    let mut conflict = f.start(&a, 2, "//:work");
    assert!(!conflict.finish());
    held.release();
    assert!(first.finish());
    let mut changed = f.start(&a, 2, "//:work");
    f.started().release();
    assert!(changed.finish());
}

#[test]
fn nested_sibling_invocations_borrow_one_slot_without_deadlocking() {
    let f = Fixture::new();
    let a = f.workspace("a", "host:database", "normal");
    let exe = aster::executor::quote_command_argument(&f.binary.display().to_string());
    let nested = format!("{exe} run //:work --quiet & {exe} run //:work --quiet; wait");
    let command = format!("sh -c {}", aster::executor::quote_command_argument(&nested));
    let mut config = fs::read_to_string(a.join("aster.toml")).unwrap();
    config.push_str(&format!("\n[targets.parent]\ncommand = {command:?}\ncache = {{ enabled = false }}\nexclusive_resources = ['host:database']\n"));
    let grandparent = format!("{exe} parent . --stream --quiet");
    config.push_str(&format!("\n[targets.grandparent]\ncommand = {grandparent:?}\ncache = {{ enabled = false }}\nexclusive_resources = ['host:database']\n"));
    fs::write(a.join("aster.toml"), config).unwrap();
    let mut parent = f.stream(&a, 1, "grandparent");
    let first = f.started();
    parent.waiting(&f.listener);
    f.remains_blocked();
    first.release();
    f.started().release();
    assert!(parent.finish());
}

#[test]
fn nested_workspace_aliases_preserve_ancestor_and_workspace_exclusion() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let a = f.workspace("a", "workspace:compiler", "normal");
    let b = f.workspace("b", "workspace:compiler", "normal");
    let c = f.workspace("c", "host:ancestor", "normal");
    let d = f.workspace("d", "workspace:compiler", "normal");
    let b_alias = f.root.path().join("b-alias");
    let host = f.root.path().join("host");
    let host_alias = f.root.path().join("host-alias");
    fs::create_dir(&host).unwrap();
    symlink(&b, &b_alias).unwrap();
    symlink(&host, &host_alias).unwrap();

    // A -> B middle -> B leaf crosses real CLI boundaries through aliases.
    // Each level reuses ancestor locks; B must also acquire its own workspace key.
    let exe = aster::executor::quote_command_argument(&f.binary.display().to_string());
    let alias = aster::executor::quote_command_argument(&b_alias.display().to_string());
    let host_alias = aster::executor::quote_command_argument(&host_alias.display().to_string());
    let mut b_config = fs::read_to_string(b.join("aster.toml")).unwrap();
    let leaf = b_config[b_config.find("[targets.work]").unwrap()..]
        .replace("[targets.work]", "[targets.leaf]")
        .replace(
            "[\"workspace:compiler\"]",
            "[\"workspace:compiler\", \"host:ancestor\", \"workspace:compiler\"]",
        );
    b_config.push_str(&leaf);
    let middle_script =
        format!("cd {alias} && ASTER_EXECUTION_DIR={host_alias} {exe} run //:leaf --quiet");
    let middle = format!(
        "sh -c {}",
        aster::executor::quote_command_argument(&middle_script)
    );
    b_config.push_str(&format!("\n[targets.middle]\ncommand = {middle:?}\ncache = {{ enabled = false }}\nexclusive_resources = ['host:ancestor', 'workspace:compiler']\n"));
    fs::write(b.join("aster.toml"), b_config).unwrap();
    let parent_script =
        format!("cd {alias} && ASTER_EXECUTION_DIR={host_alias} {exe} run //:middle --quiet");
    let parent = format!(
        "sh -c {}",
        aster::executor::quote_command_argument(&parent_script)
    );
    let mut a_config = fs::read_to_string(a.join("aster.toml")).unwrap();
    a_config.push_str(&format!("\n[targets.parent]\ncommand = {parent:?}\ncache = {{ enabled = false }}\nexclusive_resources = ['workspace:compiler', 'host:ancestor']\n"));
    fs::write(a.join("aster.toml"), a_config).unwrap();
    let mut parent = f.start(&a, 3, "//:parent");
    let held = f.started();
    assert_eq!(
        held.label, "b",
        "nested aliases could not reuse ancestor locks"
    );

    // Two spare slots rule out capacity as the reason these contenders wait.
    let mut workspace_contender = f.start(&b, 3, "//:work");
    workspace_contender.waiting(&f.listener);
    let mut ancestor_contender = f.start(&c, 3, "//:work");
    ancestor_contender.waiting(&f.listener);
    let mut independent = f.start(&d, 3, "//:work");
    let free = f.started();
    assert_eq!(
        free.label, "d",
        "a different workspace shared the compiler lock"
    );
    f.remains_blocked();
    free.release();
    assert!(independent.finish());

    // Releasing the nested leaf returns both workspace and ancestor resources.
    held.release();
    let mut released = Vec::new();
    for _ in 0..2 {
        let next = f.started();
        released.push(next.label.clone());
        next.release();
    }
    released.sort();
    assert_eq!(released, ["b", "c"]);
    assert!(parent.finish());
    assert!(workspace_contender.finish());
    assert!(ancestor_contender.finish());
}

#[test]
fn cancellation_while_waiting_never_starts_a_target_and_preserves_the_owner() {
    let f = Fixture::new();
    let a = f.workspace("a", "host:database", "normal");
    let b = f.workspace("b", "host:database", "normal");
    let mut owner = f.start(&a, 1, "//:work");
    let held = f.started();
    let mut waiting = f.start(&b, 1, "//:work");
    waiting.waiting(&f.listener);
    f.remains_blocked();
    waiting.signal(libc::SIGTERM);
    assert!(!waiting.finish());
    f.remains_blocked();
    held.release();
    assert!(owner.finish());
}

#[test]
fn cancelling_ancestor_stops_nested_work_after_its_scheduler_died() {
    let f = Fixture::new();
    let a = f.workspace("a", "", "ignoreterm");
    let b = f.workspace("b", "", "normal");
    let exe = aster::executor::quote_command_argument(&f.binary.display().to_string());
    let script = format!("{exe} run //:work --quiet & nested=$!; echo $nested > nested.pid; wait \"$nested\"; echo $? > nested.exit.tmp; mv nested.exit.tmp nested.exit");
    let command = format!("sh -c {}", aster::executor::quote_command_argument(&script));
    let mut config = fs::read_to_string(a.join("aster.toml")).unwrap();
    config.push_str(&format!(
        "\n[targets.parent]\ncommand = {command:?}\ncache = {{ enabled = false }}\n"
    ));
    fs::write(a.join("aster.toml"), config).unwrap();
    let mut parent = f.stream(&a, 1, "parent");
    let mut nested_target = f.started();
    let nested: i32 = fs::read_to_string(a.join("nested.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut next = f.start(&b, 1, "//:work");
    next.waiting(&f.listener);
    // Kill the nested scheduler so its monitor cannot forward either signal.
    assert_eq!(unsafe { libc::kill(nested, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + BUDGET;
    loop {
        match fs::read_to_string(a.join("nested.exit")) {
            Ok(status) => {
                assert_eq!(status.trim(), "137", "nested scheduler was not killed");
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                assert!(Instant::now() < deadline, "nested scheduler was not reaped");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("read nested scheduler exit: {error}"),
        }
    }
    let first_signal = Instant::now();
    parent.signal(libc::SIGTERM);
    let mut signal = [0; 5];
    nested_target.stream.read_exact(&mut signal).unwrap();
    assert_eq!(
        &signal, b"term\n",
        "ancestor cancellation did not reach the nested target"
    );
    f.remains_blocked();
    // The ancestor's second signal must cross the same surviving supervisors.
    parent.signal(libc::SIGTERM);
    assert!(!parent.finish());
    assert!(
        first_signal.elapsed() < Duration::from_secs(3),
        "ancestor force request did not beat the ordinary grace period"
    );
    let mut byte = [0];
    assert_eq!(
        nested_target.stream.read(&mut byte).unwrap(),
        0,
        "nested target survived ancestor cancellation"
    );
    f.started().release();
    assert!(next.finish());
}

#[test]
fn cached_work_bypasses_capacity_and_affected_dependencies_make_progress() {
    let f = Fixture::new();
    let a = f.workspace("a", "", "normal");
    let b = f.workspace("b", "", "normal");
    let mut config = fs::read_to_string(b.join("aster.toml")).unwrap();
    config.push_str("\n[targets.cached]\ncommand = \"sh -c 'echo ran >> count; echo result > result'\"\ncache = { enabled = true, include = ['input'], exclude = ['count', 'result'], outputs = ['result'], runtime = [] }\n");
    fs::write(b.join("aster.toml"), &config).unwrap();
    fs::write(b.join("input"), "one").unwrap();
    let mut prime = f.start(&b, 1, "//:cached");
    assert!(prime.finish());
    let mut owner = f.start(&a, 1, "//:work");
    let held = f.started();
    let mut cached = f.start(&b, 1, "//:cached");
    assert!(cached.finish(), "cached target waited on occupied capacity");
    assert_eq!(fs::read_to_string(b.join("count")).unwrap(), "ran\n");
    held.release();
    assert!(owner.finish());

    // A real affected invocation crosses Git selection, prerequisite ordering,
    // and the same admission path. Only run-owned repositories are mutated.
    config = config.replace(
        "[targets.work]",
        "[targets.work]\ndepends_on = ['//:prepare']",
    );
    config.push_str("\n[targets.prepare]\ncommand = \"sh -c 'echo prepared > prepared'\"\ncache = { enabled = false }\n");
    fs::write(b.join("aster.toml"), config).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .current_dir(&b)
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "aster.toml", "package.json", "input"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-qm",
        "fixture",
    ]);
    fs::write(b.join("input"), "changed").unwrap();
    let mut competitor = f.start(&a, 1, "//:work");
    let held = f.started();
    let mut command = f.command(&b, 1);
    command.args(["affected", "work", "--base=HEAD", "--quiet"]);
    let mut affected = Run::spawn(command);
    affected.waiting(&f.listener);
    held.release();
    assert!(competitor.finish());
    let dependent = f.started();
    assert!(
        b.join("prepared").is_file(),
        "dependent preceded its prerequisite"
    );
    dependent.release();
    assert!(affected.finish());
}

#[test]
fn multiple_ready_targets_share_two_slots_across_invocations() {
    let f = Fixture::new();
    let a = f.workspace("a", "", "normal");
    let b = f.workspace("b", "", "normal");
    for root in [&a, &b] {
        let config = fs::read_to_string(root.join("aster.toml")).unwrap();
        let extra = config[config.find("[targets.work]").unwrap()..]
            .replace("[targets.work]", "[targets.extra]");
        fs::write(root.join("aster.toml"), config + &extra).unwrap();
    }
    let start = |root: &Path| {
        let mut command = f.command(root, 2);
        command.args(["run", "//:work", "//:extra", "--quiet"]);
        Run::spawn(command)
    };
    let mut first = start(&a);
    let one = f.started();
    let two = f.started();
    let mut second = start(&b);
    second.waiting(&f.listener);
    f.remains_blocked();
    one.release();
    let three = f.started();
    f.remains_blocked();
    two.release();
    let four = f.started();
    three.release();
    four.release();
    assert!(first.finish());
    assert!(second.finish());
}

#[test]
fn delegated_interrupts_reach_target_groups_before_releasing_capacity() {
    for (streaming, second_signal, signal_number) in [
        (false, false, libc::SIGTERM),
        (false, true, libc::SIGINT),
        (true, false, libc::SIGINT),
        (true, true, libc::SIGTERM),
    ] {
        let f = Fixture::new();
        let a = f.workspace("a", "", "ignoreterm");
        let b = f.workspace("b", "", "normal");
        // Exercise --jobs after a target alias, where clap delegates parsing.
        let mut command = f.command(&a, 1);
        command
            .env_remove("ASTER_JOBS")
            .args(["work", ".", "--jobs", "1", "--quiet"]);
        if streaming {
            command.arg("--stream");
        }
        let mut owner = Run::spawn(command);
        let mut held = f.started();
        let mut next = f.start(&b, 1, "//:work");
        next.waiting(&f.listener);
        let first_signal = Instant::now();
        // Signal only the scheduling CLI. Its supervisor must bridge the
        // separate target group; the target reports TERM over the real socket.
        owner.signal(signal_number);
        let mut signal = [0; 5];
        held.stream.read_exact(&mut signal).unwrap();
        assert_eq!(&signal, b"term\n");
        f.remains_blocked();
        if second_signal {
            owner.signal(signal_number);
        }
        assert!(!owner.finish());
        if second_signal {
            // Prove the second signal beats the normal three-second grace period;
            // eventual exit alone could pass without second-signal forwarding.
            assert!(
                first_signal.elapsed() < Duration::from_secs(3),
                "second signal did not accelerate shutdown"
            );
        }
        assert_eq!(held.stream.read(&mut signal).unwrap(), 0);
        f.started().release();
        assert!(next.finish());
    }
}

#[test]
fn nested_resource_order_violation_reports_an_error_without_deadlocking() {
    let f = Fixture::new();
    let a = f.workspace("a", "host:alpha", "normal");
    let exe = aster::executor::quote_command_argument(&f.binary.display().to_string());
    let command = format!("{exe} work . --stream --quiet");
    let mut config = fs::read_to_string(a.join("aster.toml")).unwrap();
    config.push_str(&format!("\n[targets.parent]\ncommand = {command:?}\ncache = {{ enabled = false }}\nexclusive_resources = ['host:zulu']\n"));
    fs::write(a.join("aster.toml"), config).unwrap();
    let mut parent = f.stream(&a, 1, "parent");
    assert!(!parent.finish());
    let output = parent.1.try_iter().collect::<Vec<_>>().join("\n");
    assert!(
        output.contains("nested execution must acquire resources after"),
        "{output}"
    );
    f.remains_blocked();
    let mut after = f.start(&a, 1, "//:work");
    f.started().release();
    assert!(after.finish());
}

/// Canonical proof for package script chains: a chain target that holds a host
/// resource launches every step (the test runner step with its launcher and
/// the plain step after it) through the admission supervisor, so killing the
/// scheduler while a step is live does not admit a contender for the resource.
///
/// Fidelity: production CLI with `--test-results`, real `npm run`, real
/// `node --test`, the same socket barrier as above. Not proven here: Windows.
#[test]
fn a_chain_target_holding_a_resource_keeps_it_while_its_step_lives() {
    let available = |program: &str| Command::new(program).arg("--version").output().is_ok();
    if !available("node") || !available("npm") {
        eprintln!("skipping: node or npm is not installed");
        return;
    }
    let f = Fixture::new();
    let port = f.listener.local_addr().unwrap().port();
    let probe = f.root.path().join("probe.py");
    let chain_workspace = |name: &str| {
        let root = f.root.path().join(name);
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("package.json"),
            format!(
                r#"{{"name":"{name}","scripts":{{"work":"node --test t.test.js && python3 {} {port} {name} normal"}}}}"#,
                probe.display()
            ),
        )
        .unwrap();
        fs::write(
            root.join("t.test.js"),
            "require('node:test')('runs', () => {});\n",
        )
        .unwrap();
        fs::write(
            root.join("aster.toml"),
            "name = 'chain'\n[targets.work]\ncommand = 'npm run work'\ncache = { enabled = false }\nexclusive_resources = ['host:db']\n",
        )
        .unwrap();
        root
    };
    let start = |root: &Path| {
        let mut command = f.command(root, 2);
        command.args([
            "run",
            "//:work",
            "--quiet",
            &format!("--test-results={}", root.join("results.ndjson").display()),
        ]);
        Run::spawn(command)
    };
    let a = chain_workspace("a");
    let b = chain_workspace("b");
    let mut first = start(&a);
    // Step 1 (`node --test`) ran, and step 2 now holds the barrier.
    let held = f.started();
    assert_eq!(held.label, "a");
    let mut next = start(&b);
    next.waiting(&f.listener);
    f.remains_blocked();
    // Fault: kill the scheduler, not the live step or its lease owner.
    first.signal(libc::SIGKILL);
    assert!(!first.finish());
    f.remains_blocked();
    held.release();
    let started = f.started();
    assert_eq!(started.label, "b");
    started.release();
    assert!(next.finish());
    // The chain's runner step was recorded in the contender's results.
    let results = fs::read_to_string(b.join("results.ndjson")).unwrap();
    assert!(results.contains("\"runs\""), "{results}");
}

/// A nested workspace is often a sandbox with its own HOME (a test fixture that
/// runs Aster on a temp workspace) and its own job count. Before the inherited
/// context chose the host directory, the nested Aster recomputed
/// `$HOME/.cache/aster/execution-v1`, found a different directory and failed
/// every target in 0 ms with "nested execution cannot change
/// ASTER_EXECUTION_DIR".
#[test]
fn nested_workspace_with_its_own_home_and_jobs_runs_its_targets() {
    let f = Fixture::new();
    let outer = f.root.path().join("outer");
    let inner = f.root.path().join("inner");
    let outer_home = f.root.path().join("outer-home");
    let inner_home = f.root.path().join("inner-home");
    for dir in [&outer, &inner, &outer_home, &inner_home] {
        fs::create_dir(dir).unwrap();
    }
    for (dir, name) in [(&outer, "outer"), (&inner, "inner")] {
        fs::write(dir.join("package.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
    }
    fs::write(
        inner.join("aster.toml"),
        "name = 'inner'\n[targets.lint]\ncommand = 'true'\ncache = { enabled = false }\n[targets.test]\ncommand = 'true'\ncache = { enabled = false }\n",
    )
    .unwrap();
    // The nested run reports through JSON so a failure carries its reason.
    let script = format!(
        "cd {} && HOME={} ASTER_JOBS=1 {} run //:lint //:test --json > {}",
        aster::executor::quote_command_argument(&inner.display().to_string()),
        aster::executor::quote_command_argument(&inner_home.display().to_string()),
        aster::executor::quote_command_argument(&f.binary.display().to_string()),
        aster::executor::quote_command_argument(&outer.join("nested.json").display().to_string()),
    );
    fs::write(
        outer.join("aster.toml"),
        format!(
            "name = 'outer'\n[targets.nested]\ncommand = {:?}\ncache = {{ enabled = false }}\n",
            format!("sh -c {}", aster::executor::quote_command_argument(&script))
        ),
    )
    .unwrap();

    // Neither level sets ASTER_EXECUTION_DIR, as in CI: the outer state lives
    // under the outer HOME and the inner HOME differs.
    let status = Command::new(&f.binary)
        .current_dir(&outer)
        .env("HOME", &outer_home)
        .env("ASTER_JOBS", "2")
        .env_remove("ASTER_EXECUTION_DIR")
        .env_remove("ASTER_EXECUTION_CONTEXT")
        .args(["run", "//:nested", "--quiet"])
        .status()
        .unwrap();
    let nested = fs::read_to_string(outer.join("nested.json")).unwrap_or_default();
    assert!(
        status.success(),
        "outer run failed; nested output: {nested}"
    );
    assert!(
        nested.contains(r#""passed":2,"failed":0"#),
        "nested targets did not pass: {nested}"
    );
}

/// A second invocation whose `--jobs` disagrees with the active capacity fails
/// before its target starts. Human and quiet modes print the admission reason;
/// JSON keeps that reason only in `results[].output`.
///
/// Canonical proof for the conflicting-jobs human diagnostic. Fidelity: production
/// CLI, real flock host state, and the file's socket barrier (a python stand-in
/// for a long build). Not proven here: Windows (admission is disabled).
#[test]
fn conflicting_jobs_failure_names_the_reason_in_human_output() {
    let f = Fixture::new();
    let root = f.root.path().join("ws");
    fs::create_dir(&root).unwrap();
    // Workspace boundary without a parent `.git` or `aster.toml` winning the walk.
    fs::write(root.join("aster.toml"), "name = 'ws'\n").unwrap();
    let probe = aster::executor::quote_command_argument(
        &f.root.path().join("probe.py").display().to_string(),
    );
    let port = f.listener.local_addr().unwrap().port();
    for name in ["p1", "p2", "p3"] {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("package.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        let command = format!("python3 {probe} {port} {name} normal");
        fs::write(
            dir.join("aster.toml"),
            format!(
                "name = '{name}'\n[targets.build]\ncommand = {command:?}\ncache = {{ enabled = false }}\n"
            ),
        )
        .unwrap();
    }

    const REASON: &str =
        "host execution capacity is 2 while another invocation is active; requested 3";

    let mut holder = {
        let mut command = f.command(&root, 2);
        command.args(["--jobs", "2", "build", "//p1", "//p2"]);
        Run::spawn(command)
    };
    let first = f.started();
    let second = f.started();
    assert!(
        matches!(first.label.as_str(), "p1" | "p2") && matches!(second.label.as_str(), "p1" | "p2"),
        "holder admitted unexpected targets: {} {}",
        first.label,
        second.label
    );
    assert_ne!(first.label, second.label);

    let conflict = |args: &[&str]| {
        let mut command = f.command(&root, 2);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let deadline = Instant::now() + BUDGET;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("conflicting invocation did not exit: {}", args.join(" "));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut out = String::new();
        let mut err = String::new();
        stdout.read_to_string(&mut out).unwrap();
        stderr.read_to_string(&mut err).unwrap();
        (status.success(), out, err)
    };

    let (ok, _stdout, stderr) = conflict(&["--jobs", "3", "build", "//p3"]);
    assert!(!ok, "human conflict should fail: {stderr}");
    assert!(
        stderr.contains(REASON),
        "human stderr missing admission reason: {stderr}"
    );
    assert!(
        !stderr.contains("START"),
        "conflict reported a target start: {stderr}"
    );

    let (ok, stdout, stderr) = conflict(&["--jobs", "3", "--json", "build", "//p3"]);
    assert!(!ok, "json conflict should fail: {stderr}\n{stdout}");
    assert!(
        !stderr.contains(REASON),
        "json mode printed the admission reason on stderr: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("json stdout was not the execution document: {error}\n{stdout}")
    });
    assert!(
        json.get("results").and_then(|v| v.as_array()).is_some() && json.get("summary").is_some(),
        "json schema changed: {stdout}"
    );
    let outputs: Vec<_> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["output"].as_str())
        .collect();
    assert!(
        outputs.iter().any(|output| output.contains(REASON)),
        "json results[].output missing admission reason: {stdout}"
    );
    assert_eq!(
        json["summary"]["failed"].as_u64(),
        Some(1),
        "json failure count changed: {stdout}"
    );

    let (ok, _stdout, stderr) = conflict(&["--jobs", "3", "-q", "build", "//p3"]);
    assert!(!ok, "quiet conflict should fail: {stderr}");
    assert!(
        stderr.contains(REASON),
        "quiet mode lost the admission reason: {stderr}"
    );
    assert!(
        !stderr.contains("FAILED") && !stderr.contains("START"),
        "quiet mode printed per-target noise: {stderr}"
    );

    // None of the conflicting invocations may have started //p3.
    f.remains_blocked();

    // Same --jobs waits for a free slot and must not print the conflict reason.
    let mut same = {
        let mut command = f.command(&root, 2);
        command.args(["--jobs", "2", "build", "//p3"]);
        Run::spawn(command)
    };
    let deadline = Instant::now() + BUDGET;
    let mut seen = String::new();
    loop {
        assert!(
            matches!(f.listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "same-jobs target started before a slot was free"
        );
        match same.1.try_recv() {
            Ok(line) => {
                assert!(
                    !line.contains("while another invocation is active"),
                    "same jobs printed a capacity conflict: {line}"
                );
                seen.push_str(&line);
                seen.push('\n');
                if line.contains("Waiting for execution admission:") {
                    break;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(error) => panic!("same-jobs invocation exited before admission: {error}; {seen}"),
        }
        assert!(
            Instant::now() < deadline,
            "same-jobs invocation did not reach admission: {seen}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    first.release();
    second.release();
    let started = f.started();
    assert_eq!(started.label, "p3");
    started.release();
    assert!(same.finish(), "same-jobs run failed after a slot freed");
    assert!(holder.finish(), "holder failed after releasing its targets");
}

fn child_pids(parent: u32) -> Vec<u32> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .expect("ps");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let ppid = parts.next()?.parse::<u32>().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

fn process_group(pid: u32) -> i32 {
    unsafe { libc::getpgid(pid as i32) }
}

fn sigkill_not_self_group(pid: u32) {
    let group = process_group(pid);
    let mine = unsafe { libc::getpgrp() };
    if group > 1 && group != mine {
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn wait_until_dead(pids: &[u32]) {
    let deadline = Instant::now() + BUDGET;
    for pid in pids {
        loop {
            let alive = unsafe { libc::kill(*pid as i32, 0) } == 0;
            if !alive || process_state(*pid).contains('Z') {
                break;
            }
            assert!(Instant::now() < deadline, "pid {pid} survived SIGKILL");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn process_state(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    output
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn lock_is_free(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn wait_until_free(path: &Path) {
    let deadline = Instant::now() + BUDGET;
    loop {
        if lock_is_free(path) {
            return;
        }
        assert!(
            path.exists(),
            "lock disappeared before it was free: {}",
            path.display()
        );
        assert!(
            Instant::now() < deadline,
            "lock stayed held: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn family_dirs(host: &Path) -> Vec<PathBuf> {
    let mut dirs = fs::read_dir(host)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("family-"))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

/// Canonical proof that a killed admitted run does not leak its family directory.
///
/// Fidelity: real `aster work`, real flocks, real SIGKILL of the scheduler,
/// supervisor, and target groups. The next invocation on the same
/// ASTER_EXECUTION_DIR removes the stale family and leaves a concurrently live
/// run's family in place. Not proven here: NFS locks.
#[test]
fn killed_admitted_run_leaves_no_family_dir_after_next_invocation() {
    let f = Fixture::new();
    let killed_ws = f.workspace("killed", "", "normal");
    let live_ws = f.workspace("live", "", "normal");
    let next_ws = f.workspace("next", "", "normal");
    let host = f.root.path().join("host");
    let work = |root: &Path| {
        let mut command = f.command(root, 1);
        command.args(["work", ".", "--quiet"]);
        Run::spawn(command)
    };

    let mut killed = work(&killed_ws);
    let killed_target = f.started();
    assert_eq!(killed_target.label, "killed");
    let stale = {
        let families = family_dirs(&host);
        assert_eq!(
            families.len(),
            1,
            "admitted run did not create one family dir"
        );
        families.into_iter().next().unwrap()
    };

    // Reach configured() while the first family is still live, then wait. This
    // invocation must not be the one that sweeps after the kill.
    let mut live = work(&live_ws);
    live.waiting(&f.listener);

    let scheduler = killed.0.id();
    let supervisors = child_pids(scheduler);
    assert!(
        !supervisors.is_empty(),
        "execution supervisor was not spawned"
    );
    let mut targets = Vec::new();
    for supervisor in &supervisors {
        targets.extend(child_pids(*supervisor));
    }
    assert!(!targets.is_empty(), "admitted target was not spawned");
    let supervisor = supervisors[0];
    let target = targets[0];
    assert_ne!(
        process_group(supervisor),
        process_group(target),
        "target was not in its own process group"
    );
    assert_ne!(
        process_group(supervisor),
        unsafe { libc::getpgrp() },
        "supervisor stayed in the test process group"
    );

    // Kill the scheduler before its supervisor. A live scheduler would drop the
    // family directory itself; a live supervisor would remove it after the target
    // died. Neither destructor may run.
    assert_eq!(unsafe { libc::kill(scheduler as i32, libc::SIGKILL) }, 0);
    for pid in &supervisors {
        sigkill_not_self_group(*pid);
    }
    for pid in &targets {
        sigkill_not_self_group(*pid);
    }
    let _ = killed.0.wait();
    let mut killed_pids = vec![scheduler];
    killed_pids.extend(supervisors);
    killed_pids.extend(targets);
    wait_until_dead(&killed_pids);
    assert!(
        stale.is_dir(),
        "killed run's family dir was removed before the next invocation"
    );
    wait_until_free(&stale.join("owner.lock"));
    wait_until_free(&stale.join("borrow.lock"));
    assert!(
        stale.is_dir(),
        "family dir disappeared while its locks were released"
    );

    let live_target = f.started();
    assert_eq!(live_target.label, "live");
    let families = family_dirs(&host);
    assert!(
        families.contains(&stale),
        "the invocation that was already waiting swept the killed family: {families:?}"
    );
    let live_family = families
        .iter()
        .find(|path| *path != &stale)
        .cloned()
        .expect("live run did not create a family dir");

    let preserved = ["authority.lock", "epoch.lock", "capacity", "slot-0.lock"];
    let before: Vec<_> = preserved
        .iter()
        .map(|name| {
            let path = host.join(name);
            (
                *name,
                fs::read(&path).unwrap_or_else(|error| panic!("read {name}: {error}")),
            )
        })
        .collect::<Vec<_>>();

    let mut next = work(&next_ws);
    next.waiting(&f.listener);
    assert!(!stale.exists(), "next invocation left the stale family dir");
    assert!(
        live_family.is_dir(),
        "next invocation removed a live run's family dir"
    );
    for (name, bytes) in before {
        let path = host.join(name);
        assert!(path.exists(), "{name} was removed by the sweep");
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "{name} was rewritten by the sweep"
        );
    }

    live_target.release();
    assert!(live.finish());
    let next_target = f.started();
    assert_eq!(next_target.label, "next");
    next_target.release();
    assert!(next.finish());
}
