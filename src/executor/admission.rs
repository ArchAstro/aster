//! Host execution admission for the CLI. File locks, not PID files or clocks,
//! own capacity. An exec'd command supervisor retains them if its scheduler dies.

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
