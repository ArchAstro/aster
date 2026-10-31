//! Real CLI/process proofs of host admission. Each test has its own host state;
//! no test acquires a developer's execution slots or signals unrelated jobs.
//!
//! Canonical proof: concurrent_workspaces_share_capacity_and_release_after_child_exit.
//! Integration matrix: run/affected/streaming; one/multiple ready targets; one/two
//! workspaces and invocations; equal/different host keys; same/different workspace
//! keys; cold/warm cache; normal/failure/waiting cancellation/scheduler SIGKILL;
//! recursive nested siblings; workspace/state symlink aliases; inherited/new
//! resources; captured/streaming first/second interruption of stubborn children.
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
