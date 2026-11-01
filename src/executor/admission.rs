//! Host execution admission for the CLI. File locks, not PID files or clocks,
//! own capacity. An exec'd command supervisor retains them if its scheduler dies.
//! The authority holder removes family directories whose owner and borrow locks
//! are free. A family that has not created those locks yet is kept until the
//! directory is older than ten minutes, so a concurrent acquire is not deleted.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const CONTEXT_ENV: &str = "ASTER_EXECUTION_CONTEXT";
const SPEC_ENV: &str = "ASTER_EXECUTION_SPEC";
const INTERNAL_COMMAND: &str = "__aster_execute";
/// Longer than the window in which acquire creates a family directory and its
/// lock files, and short enough that a killed acquire does not leak forever.
const STALE_FAMILY_GRACE: Duration = Duration::from_secs(10 * 60);
static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

/// Enable host admission when embedding the executor in an executable which
/// dispatches [`internal_invocation`] before normal CLI parsing.
pub fn enable(executable: PathBuf) {
    let _ = EXECUTABLE.set(executable);
}

#[derive(Clone, Serialize, Deserialize)]
struct Family {
    directory: PathBuf,
    resources: Vec<String>,
    ancestors: Vec<PathBuf>,
}

pub(super) struct Host {
    directory: PathBuf,
    capacity: usize,
    epoch: File,
    inherited: Option<Family>,
    executable: PathBuf,
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open execution lock {}", path.display()))
}

fn try_lock(file: &File) -> Result<bool> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn open_existing(path: &Path) -> std::io::Result<Option<File>> {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

enum LockPresence {
    Missing,
    Free,
    Held,
}

/// `None` means skip this family: the lock file could not be inspected.
/// A free lock is pushed onto `held` and stays locked until that vector is dropped.
fn inspect_lock(path: &Path, held: &mut Vec<File>) -> Option<LockPresence> {
    let Some(file) = open_existing(path).ok()? else {
        return Some(LockPresence::Missing);
    };
    match try_lock(&file) {
        Ok(true) => {
            held.push(file);
            Some(LockPresence::Free)
        }
        Ok(false) => Some(LockPresence::Held),
        Err(_) => None,
    }
}

fn directory_older_than_grace(path: &Path) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    modified.elapsed().is_ok_and(|age| age > STALE_FAMILY_GRACE)
}

fn reclaim_stale_family(path: &Path) -> Result<()> {
    // Hold owner across the borrow check and the removal. Dropping it first
    // lets acquire lock owner and create an unlocked borrow, which then looks
    // dead. Acquire does not hold authority.lock.
    let mut held = Vec::new();
    let Some(owner) = inspect_lock(&path.join("owner.lock"), &mut held) else {
        return Ok(());
    };
    if matches!(owner, LockPresence::Held) {
        return Ok(());
    }
    let Some(borrow) = inspect_lock(&path.join("borrow.lock"), &mut held) else {
        return Ok(());
    };
    if matches!(borrow, LockPresence::Held) {
        return Ok(());
    }
    let both_free = matches!((owner, borrow), (LockPresence::Free, LockPresence::Free));
    // A missing lock file is not immediate proof of death: acquire creates the
    // directory before either file. Keep it inside the grace period.
    if !both_free && !directory_older_than_grace(path) {
        return Ok(());
    }
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("remove stale execution family {}", path.display()))
        }
    }
    drop(held);
    Ok(())
}

fn sweep_stale_families(directory: &Path) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read execution directory {}", directory.display()))
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read execution directory {}", directory.display()))
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("family-") {
            continue;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("stat execution family {}", path.display()))
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        reclaim_stale_family(&path)?;
    }
    Ok(())
}

fn wait() -> Result<()> {
    if super::shutdown_requested() {
        bail!("Cancelled: shutdown requested while waiting for execution admission");
    }
    // Poll OS lock availability, never infer ownership from elapsed time.
    std::thread::sleep(Duration::from_millis(20));
    Ok(())
}

fn directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ASTER_EXECUTION_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("ASTER_EXECUTION_DIR must be absolute");
        }
        return Ok(path);
    }
    // TMPDIR commonly belongs to one job and must not divide the host budget.
    let home = std::env::var_os("HOME").context("HOME is required for host execution admission")?;
    Ok(PathBuf::from(home).join(".cache/aster/execution-v1"))
}

impl Host {
    pub(super) fn configured() -> Result<Option<Arc<Self>>> {
        let Some(executable) = EXECUTABLE.get() else {
            return Ok(None);
        };
        // A nested invocation joins the host state of the ancestor that admitted
        // it. HOME and ASTER_EXECUTION_DIR describe where a fresh invocation
        // keeps its state; a nested workspace may legitimately differ (a test
        // sandbox with its own HOME), and a different directory could not see
        // the ancestor's locks. The inherited context is authoritative.
        let inherited = std::env::var(CONTEXT_ENV)
            .ok()
            .map(|s| serde_json::from_str::<Family>(&s))
            .transpose()
            .context("invalid inherited execution context")?;
        let directory = match &inherited {
            Some(family) => family
                .directory
                .parent()
                .context("inherited execution context has no host directory")?
                .to_path_buf(),
            None => directory()?,
        };
        fs::create_dir_all(&directory)?;
        let directory = fs::canonicalize(directory)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let requested = std::env::var("ASTER_JOBS")
            .ok()
            .map(|n| {
                n.parse::<std::num::NonZeroUsize>()
                    .context("ASTER_JOBS/--jobs must be a positive integer")
                    .map(|n| n.get())
            })
            .transpose()?;
        let authority = open_lock(&directory.join("authority.lock"))?;
        while !try_lock(&authority)? {
            wait()?;
        }
        let epoch = open_lock(&directory.join("epoch.lock"))?;
        let idle = try_lock(&epoch)?;
        // Lock tests decide liveness, so this runs on a busy epoch too. Hold
        // authority.lock across the sweep so a new invocation cannot pass
        // configured() until it finishes. An acquire already past that point
        // does not hold this lock; missing lock files use the grace period.
        sweep_stale_families(&directory)?;
        let settings = directory.join("capacity");
        let previous = match fs::read_to_string(&settings) {
            Ok(text) => Some(
                text.parse::<usize>()
                    .context("invalid host execution capacity")?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let capacity = if idle {
            let value = requested.or(previous).unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(1, |n| n.get().min(4))
            });
            let mut next = tempfile::NamedTempFile::new_in(&directory)?;
            write!(next, "{value}")?;
            next.as_file().sync_all()?;
            next.persist(&settings)?;
            value
        } else {
            let value = previous.context("active execution epoch has no capacity")?;
            // A nested invocation borrows its ancestor's slot and never takes
            // one of its own, so its requested job count cannot conflict.
            if inherited.is_none() && requested.is_some_and(|n| n != value) {
                bail!("host execution capacity is {value} while another invocation is active; requested {}. Use the active capacity or change it when all invocations finish", requested.unwrap());
            }
            value
        };
        if capacity == 0 {
            bail!("host execution capacity must be positive");
        }
        // The authority lock makes conversion safe even where flock's
        // exclusive-to-shared conversion is not atomic.
        FileExt::lock_shared(&epoch)?;
        drop(authority);
        Ok(Some(Arc::new(Self {
            directory,
            capacity,
            epoch,
            inherited,
            executable: executable.clone(),
        })))
    }

    pub(super) fn acquire(
        &self,
        resources: &[String],
        workspace: &Path,
        address: &str,
    ) -> Result<Permit> {
        let workspace = fs::canonicalize(workspace)?;
        let mut keys = resources
            .iter()
            .filter_map(|name| {
                if name.starts_with("host:") {
                    Some(name.clone())
                } else {
                    name.strip_prefix("workspace:").map(|name| {
                        format!(
                            "workspace:{}:{}:{name}",
                            workspace.as_os_str().len(),
                            workspace.display()
                        )
                    })
                }
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        let mut inherited_resources = Vec::new();
        if let Some(family) = &self.inherited {
            inherited_resources.clone_from(&family.resources);
            // An ancestor already owns these resources. New acquisitions must
            // follow the same total ordering to prevent a nested lock cycle.
            if let Some(last) = inherited_resources.last() {
                if keys
                    .iter()
                    .any(|k| !inherited_resources.contains(k) && k < last)
                {
                    bail!("nested execution must acquire resources after {last} in lexical order");
                }
            }
            keys.retain(|k| !inherited_resources.contains(k));
        }
        let mut reported_wait = false;
        loop {
            if super::shutdown_requested() {
                bail!("Cancelled: shutdown requested while waiting for execution admission");
            }
            let mut files = vec![self.epoch.try_clone()?];
            let mut ready = true;
            // Acquire the whole set nonblocking. Never hold a resource while
            // waiting for capacity, or a partial set while another caller runs.
            for key in &keys {
                use sha2::{Digest, Sha256};
                let digest = Sha256::digest(key.as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let name = format!("resource-{digest}.lock");
                let file = open_lock(&self.directory.join(name))?;
                if !try_lock(&file)? {
                    ready = false;
                    break;
                }
                files.push(file);
            }
            if ready {
                if let Some(family) = &self.inherited {
                    let owner = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(family.directory.join("owner.lock"))
                        .context("inherited execution owner is gone")?;
                    if try_lock(&owner)? {
                        bail!("inherited execution owner is gone");
                    }
                    let borrow = open_lock(&family.directory.join("borrow.lock"))?;
                    if try_lock(&borrow)? {
                        files.push(borrow);
                    } else {
                        ready = false;
                    }
                } else {
                    ready = false;
                    for n in 0..self.capacity {
                        let file = open_lock(&self.directory.join(format!("slot-{n}.lock")))?;
                        if try_lock(&file)? {
                            files.push(file);
                            ready = true;
                            break;
                        }
                    }
                }
            }
            if ready {
                let family_dir = tempfile::Builder::new()
                    .prefix("family-")
                    .tempdir_in(&self.directory)?;
                let owner = open_lock(&family_dir.path().join("owner.lock"))?;
                owner.lock_exclusive()?;
                files.push(owner);
                open_lock(&family_dir.path().join("borrow.lock"))?;
                inherited_resources.extend(keys);
                let family = Family {
                    directory: family_dir.path().to_path_buf(),
                    resources: inherited_resources,
                    ancestors: self
                        .inherited
                        .as_ref()
                        .map(|parent| {
                            let mut ancestors = parent.ancestors.clone();
                            ancestors.push(parent.directory.clone());
                            ancestors
                        })
                        .unwrap_or_default(),
                };
                return Ok(Permit {
                    files,
                    family,
                    _directory: family_dir,
                    executable: self.executable.clone(),
                });
            }
            drop(files);
            if !reported_wait {
                eprintln!(
                    "[aster] Waiting for execution admission: {address} (host capacity {})",
                    self.capacity
                );
                reported_wait = true;
            }
            wait()?;
        }
    }
}

pub(super) struct Permit {
    files: Vec<File>,
    family: Family,
    _directory: tempfile::TempDir,
    executable: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Spec {
    descriptors: Vec<i32>,
    family: Family,
    program: String,
    args: Vec<String>,
    scheduler_pid: u32,
}

impl Permit {
    pub(super) fn command(&self, program: &str, args: &[String]) -> Result<Command> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let descriptors = self
            .files
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        let spec = Spec {
            descriptors: descriptors.clone(),
            family: self.family.clone(),
            program: program.into(),
            args: args.to_vec(),
            scheduler_pid: std::process::id(),
        };
        let mut command = Command::new(&self.executable);
        command
            .arg(INTERNAL_COMMAND)
            .env(SPEC_ENV, serde_json::to_string(&spec)?);
        // Only this spawn inherits these descriptors. Never clear CLOEXEC in
        // the multithreaded parent, where another target could inherit them.
        unsafe {
            command.pre_exec(move || {
                for fd in &descriptors {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(command)
    }
}

/// Dispatch the private exec'd supervisor before parsing the public CLI.
/// The target itself may close every inherited descriptor: its supervisor
/// still owns the locks until the target group and nested borrowers exit.
pub fn internal_invocation() -> Option<Result<i32>> {
    if std::env::args().nth(1).as_deref() != Some(INTERNAL_COMMAND) {
        return None;
    }
    Some(supervise())
}

fn supervise() -> Result<i32> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let spec: Spec = serde_json::from_str(
        &std::env::var(SPEC_ENV).context("missing execution supervisor specification")?,
    )?;
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for fd in spec.descriptors {
        if !seen.insert(fd) {
            bail!("duplicate execution lease descriptor");
        }
        if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            bail!("invalid execution lease descriptor");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        // The helper owns the leases. Its target need not cooperate with FD inheritance.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        files.push(file);
    }
    super::install_signal_handler();
    super::request_graceful_signal_handling();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_remove(SPEC_ENV)
        .env(CONTEXT_ENV, serde_json::to_string(&spec.family)?)
        .process_group(0);
    let borrow = open_lock(&spec.family.directory.join("borrow.lock"))?;
    let ancestor_request = || -> Result<(bool, bool)> {
        let mut cancel = false;
        let mut force = false;
        for ancestor in &spec.family.ancestors {
            cancel |= ancestor.join("cancel").try_exists()?;
            force |= ancestor.join("force").try_exists()?;
        }
        Ok((cancel, force))
    };
    let (cancel, force) = ancestor_request()?;
    if super::shutdown_requested() || cancel || force {
        return Ok(143);
    }
    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!(
            "spawn admitted target: {}",
            super::spawn_error_message(&spec.program, &error)
        )
    })?;
    let group = child.id() as i32;
    let mut status = None;
    let mut terminating = None;
    let mut control_error = false;
    let mut wait_failed = false;
    let mut forced = false;
    loop {
        // Ancestor requests remain observable if an intermediate scheduler
        // has died. They are cancellation intent, never a liveness heuristic.
        let (inherited_cancel, inherited_force) = match ancestor_request() {
            Ok(request) => request,
            Err(error) => {
                if !control_error {
                    eprintln!("cannot read ancestor cancellation: {error}");
                }
                control_error = true;
                (true, false)
            }
        };
        if super::shutdown_requested() || inherited_cancel || inherited_force || control_error {
            let since = terminating.get_or_insert_with(|| {
                if let Err(error) = fs::write(spec.family.directory.join("cancel"), []) {
                    eprintln!("cannot propagate execution cancellation: {error}");
                    control_error = true;
                }
                unsafe {
                    libc::kill(-group, libc::SIGTERM);
                }
                std::time::Instant::now()
            });
            if !forced
                && (super::force_shutdown_requested()
                    || inherited_force
                    || since.elapsed() >= Duration::from_secs(3))
            {
                forced = true;
                if let Err(error) = fs::write(spec.family.directory.join("force"), []) {
                    eprintln!("cannot propagate forced execution cancellation: {error}");
                    control_error = true;
                }
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
        }
        // After spawning, no fallible operation may drop the leases while
        // work is alive. An observation error requests cancellation and keeps
        // ownership until the group and nested borrowers are known to be gone.
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => {
                    if !wait_failed {
                        eprintln!("cannot observe admitted target: {error}");
                    }
                    wait_failed = true;
                    control_error = true;
                }
            }
        }
        let group_alive = unsafe { libc::kill(-group, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if (status.is_some() || wait_failed) && !group_alive {
            match try_lock(&borrow) {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => {
                    if !control_error {
                        eprintln!("cannot observe nested borrowers: {error}");
                    }
                    control_error = true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = status.context("target did not exit")?;
    drop(files);
    // The kernel parent relationship proves the original scheduler is gone;
    // its PID's existence alone would not. A live scheduler owns normal cleanup.
    if unsafe { libc::getppid() } as u32 != spec.scheduler_pid {
        let _ = fs::remove_dir_all(&spec.family.directory);
    }
    if control_error {
        bail!("execution supervision failed");
    }
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::SystemTime;

    fn host() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn family(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir(&path).unwrap();
        path
    }

    fn touch_locks(path: &Path) {
        open_lock(&path.join("owner.lock")).unwrap();
        open_lock(&path.join("borrow.lock")).unwrap();
    }

    fn set_directory_mtime(path: &Path, time: SystemTime) {
        File::open(path)
            .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
            .set_modified(time)
            .unwrap_or_else(|error| panic!("set mtime {}: {error}", path.display()));
    }

    /// Holds an exclusive flock in another process. Same-process flock does not
    /// conflict on every Unix, and production holders are other processes.
    struct HeldLock(std::process::Child);

    impl HeldLock {
        fn exclusive(path: &Path) -> Self {
            let mut child = Command::new("python3")
                .arg("-u")
                .arg("-c")
                .arg(
                    "import fcntl, os, sys, time\n\
fd = os.open(sys.argv[1], os.O_RDWR)\n\
fcntl.flock(fd, fcntl.LOCK_EX)\n\
sys.stdout.buffer.write(b'held\\n')\n\
sys.stdout.buffer.flush()\n\
time.sleep(120)\n",
                )
                .arg(path)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 is required to hold a lock from another process");
            let mut stdout = child.stdout.take().unwrap();
            let mut buf = [0; 5];
            stdout
                .read_exact(&mut buf)
                .expect("lock holder did not acquire the lock");
            assert_eq!(&buf, b"held\n");
            Self(child)
        }
    }

    impl Drop for HeldLock {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn sweep_removes_dead_families_and_keeps_held_locks() {
        let dir = host();
        let dead = family(dir.path(), "family-dead");
        touch_locks(&dead);
        let owned = family(dir.path(), "family-owned");
        touch_locks(&owned);
        let borrowed = family(dir.path(), "family-borrowed");
        touch_locks(&borrowed);
        let owner_only = family(dir.path(), "family-owner-only");
        open_lock(&owner_only.join("owner.lock")).unwrap();
        set_directory_mtime(
            &owner_only,
            SystemTime::now() - Duration::from_secs(11 * 60),
        );
        let _owner = HeldLock::exclusive(&owned.join("owner.lock"));
        let _borrow = HeldLock::exclusive(&borrowed.join("borrow.lock"));
        let _owner_only = HeldLock::exclusive(&owner_only.join("owner.lock"));

        sweep_stale_families(dir.path()).unwrap();

        assert!(!dead.exists(), "dead family was kept");
        assert!(owned.exists(), "live owner.lock was removed");
        assert!(borrowed.exists(), "live borrow.lock was removed");
        assert!(
            owner_only.exists(),
            "held owner.lock was removed because borrow.lock was missing and old"
        );
    }

    #[test]
    fn sweep_keeps_a_recent_family_without_lock_files_until_the_grace_period() {
        let dir = host();
        let fresh = family(dir.path(), "family-fresh");
        let partial = family(dir.path(), "family-partial");
        open_lock(&partial.join("owner.lock")).unwrap();
        sweep_stale_families(dir.path()).unwrap();
        assert!(fresh.exists(), "family without lock files was removed");
        assert!(
            partial.exists(),
            "family whose borrow.lock was not created yet was removed"
        );

        let stale_at = SystemTime::now() - Duration::from_secs(11 * 60);
        set_directory_mtime(&fresh, stale_at);
        set_directory_mtime(&partial, stale_at);
        sweep_stale_families(dir.path()).unwrap();
        assert!(
            !fresh.exists(),
            "family without lock files survived the grace period"
        );
        assert!(
            !partial.exists(),
            "old family with a missing lock file was kept"
        );
    }

    #[test]
    fn sweep_leaves_admission_lock_files_untouched() {
        let dir = host();
        let dead = family(dir.path(), "family-dead");
        touch_locks(&dead);
        let kept = [
            "slot-0.lock",
            "slot-1.lock",
            "epoch.lock",
            "authority.lock",
            "resource-abc.lock",
            "capacity",
            "family-not-a-dir",
        ];
        for name in kept {
            fs::write(dir.path().join(name), name).unwrap();
        }

        sweep_stale_families(dir.path()).unwrap();

        assert!(!dead.exists());
        for name in kept {
            let path = dir.path().join(name);
            assert_eq!(
                fs::read(&path).unwrap_or_else(|error| panic!("read {name}: {error}")),
                name.as_bytes(),
                "{name} was removed or rewritten"
            );
        }
    }

    #[test]
    fn two_threads_wait_for_authority_lock_before_sweeping() {
        let dir = host();
        let dead = family(dir.path(), "family-dead");
        touch_locks(&dead);
        let authority = dir.path().join("authority.lock");
        open_lock(&authority).unwrap();
        let holder = HeldLock::exclusive(&authority);
        let entered = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = dir.path().to_path_buf();
            let entered = Arc::clone(&entered);
            let blocked = Arc::clone(&blocked);
            handles.push(std::thread::spawn(move || {
                let authority = open_lock(&path.join("authority.lock")).unwrap();
                entered.fetch_add(1, Ordering::SeqCst);
                let mut reported = false;
                while !try_lock(&authority).unwrap() {
                    if !reported {
                        blocked.fetch_add(1, Ordering::SeqCst);
                        reported = true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                sweep_stale_families(&path).unwrap();
                drop(authority);
            }));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while blocked.load(Ordering::SeqCst) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "sweep threads did not block on authority.lock (started {})",
                entered.load(Ordering::SeqCst)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dead.exists(),
            "sweep removed a family while another process held authority.lock"
        );
        drop(holder);
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(
            !dead.exists(),
            "sweep did not run after authority.lock was released"
        );
    }
}
