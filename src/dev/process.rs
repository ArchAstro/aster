use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::executor::command::parse_command;
#[cfg(any(unix, windows))]
use crate::executor::{register_supervised_child, unregister_supervised_child};
use crate::plugins::Target;

#[derive(Debug)]
pub struct LogEvent {
    pub service: String,
    pub line: String,
    pub stderr: bool,
}

pub struct ServiceProcess {
    child: Child,
    #[cfg(unix)]
    pgid: i32,
    stdout: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
    reader_stop: Arc<AtomicBool>,
    finished: bool,
    #[cfg(any(unix, windows))]
    registered: bool,
    #[cfg(windows)]
    job: usize,
}

impl ServiceProcess {
    pub fn spawn(
        service: &str,
        target: &Target,
        project_root: &std::path::Path,
        env: &HashMap<String, String>,
        log_tx: &SyncSender<LogEvent>,
        system_tx: &Sender<LogEvent>,
        ui: bool,
    ) -> Result<Self> {
        let parsed = parse_command(&target.command)?;
        let working_dir = target.working_dir.as_deref().unwrap_or(project_root);
        let mut command = Command::new(&parsed.program);
        command
            .args(&parsed.args)
            .current_dir(working_dir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        preserve_baseline_environment(&mut command);
        command.envs(env);
        for (name, value) in parsed.env {
            command.env(name, value);
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            register_supervised_child();
        }
        #[cfg(windows)]
        register_supervised_child();

        let mut child = command
            .spawn()
            .inspect_err(|_| {
                #[cfg(any(unix, windows))]
                unregister_supervised_child();
            })
            .with_context(|| format!("failed to spawn service '{service}'"))?;
        #[cfg(windows)]
        let job = assign_kill_on_close_job_and_resume(&mut child)
            .inspect_err(|_| unregister_supervised_child())
            .with_context(|| format!("failed to supervise service '{service}'"))?;
        let stdout = child.stdout.take().context("missing service stdout")?;
        let stderr = child.stderr.take().context("missing service stderr")?;
        #[cfg(unix)]
        if let Err(error) =
            set_pipe_nonblocking(&stdout).and_then(|()| set_pipe_nonblocking(&stderr))
        {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            unregister_supervised_child();
            return Err(error);
        }
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stdout_handle = spawn_reader(
            service,
            false,
            stdout,
            log_tx.clone(),
            system_tx.clone(),
            reader_stop.clone(),
            ui,
        );
        let stderr_handle = spawn_reader(
            service,
            true,
            stderr,
            log_tx.clone(),
            system_tx.clone(),
            reader_stop.clone(),
            ui,
        );

        Ok(Self {
            #[cfg(unix)]
            pgid: child.id() as i32,
            child,
            stdout: Some(stdout_handle),
            stderr: Some(stderr_handle),
            reader_stop,
            finished: false,
            #[cfg(unix)]
            registered: true,
            #[cfg(windows)]
            registered: true,
            #[cfg(windows)]
            job,
        })
    }

    pub fn poll(&mut self) -> Result<Option<i32>> {
        Ok(self
            .child
            .try_wait()?
            .map(|status| status.code().unwrap_or(-1)))
    }

    pub fn terminate(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        self.request_terminate();
        self.finish_terminate(deadline);
    }

    pub fn request_terminate(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pgid, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            if self.job == 0 {
                crate::windows_process::terminate_process_tree(self.child.id());
            } else {
                unsafe {
                    windows_sys::Win32::System::JobObjects::TerminateJobObject(
                        self.job as windows_sys::Win32::Foundation::HANDLE,
                        1,
                    );
                }
            }
            #[cfg(not(windows))]
            let _ = self.child.kill();
        }
    }

    pub fn finish_terminate(&mut self, deadline: Instant) {
        while Instant::now() < deadline {
            let child_exited = self.child.try_wait().ok().flatten().is_some();
            #[cfg(unix)]
            let group_exited = !process_group_exists(self.pgid);
            #[cfg(not(unix))]
            let group_exited = child_exited;
            if child_exited && group_exited {
                self.finish();
                return;
            }
            std::thread::sleep(Duration::from_millis(40));
        }

        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }
        #[cfg(windows)]
        if self.job == 0 {
            crate::windows_process::terminate_process_tree(self.child.id());
        } else {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.job as windows_sys::Win32::Foundation::HANDLE,
                    1,
                );
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        {
            let force_deadline = Instant::now() + Duration::from_secs(1);
            while process_group_exists(self.pgid) && Instant::now() < force_deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        self.finish();
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        #[cfg(windows)]
        let job_backed = self.job != 0;
        #[cfg(not(windows))]
        let job_backed = true;
        #[cfg(windows)]
        if !job_backed {
            crate::windows_process::terminate_process_tree(self.child.id());
        }
        #[cfg(windows)]
        if self.job != 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(
                    self.job as windows_sys::Win32::Foundation::HANDLE,
                );
            }
            self.job = 0;
        }
        if job_backed {
            let drain_deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < drain_deadline
                && (self
                    .stdout
                    .as_ref()
                    .is_some_and(|handle| !handle.is_finished())
                    || self
                        .stderr
                        .as_ref()
                        .is_some_and(|handle| !handle.is_finished()))
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            self.reader_stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.stdout.take() {
                let _ = handle.join();
            }
            if let Some(handle) = self.stderr.take() {
                let _ = handle.join();
            }
        } else {
            self.reader_stop.store(true, Ordering::SeqCst);
            self.stdout.take();
            self.stderr.take();
        }
        #[cfg(any(unix, windows))]
        if self.registered {
            unregister_supervised_child();
            self.registered = false;
        }
        self.finished = true;
    }
}

fn preserve_baseline_environment(command: &mut Command) {
    #[cfg(unix)]
    const BASELINE: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "TERM",
    ];
    #[cfg(windows)]
    const BASELINE: &[&str] = &[
        "PATH",
        "SystemRoot",
        "WINDIR",
        "ComSpec",
        "PATHEXT",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "LANG",
    ];
    #[cfg(not(any(unix, windows)))]
    const BASELINE: &[&str] = &["PATH", "HOME", "TMPDIR", "TEMP", "TMP", "LANG"];

    for key in BASELINE {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LC_") {
            command.env(key, value);
        }
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        #[cfg(unix)]
        let group_exists = process_group_exists(self.pgid);
        #[cfg(not(unix))]
        let group_exists = self.child.try_wait().ok().flatten().is_none();
        if group_exists {
            self.terminate(Duration::from_millis(500));
        } else {
            self.finish();
        }
    }
}

#[cfg(unix)]
fn process_group_exists(pgid: i32) -> bool {
    let result = unsafe { libc::kill(-pgid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn set_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        anyhow::bail!(
            "failed to make service output pipe nonblocking: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn assign_kill_on_close_job_and_resume(child: &mut Child) -> Result<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = windows_sys::Win32::System::JobObjects::CreateJobObjectW(
            std::ptr::null(),
            std::ptr::null(),
        );
        if job.is_null() {
            let error = std::io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("CreateJobObjectW failed: {error}");
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of_val(&info) as u32,
        );
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            windows_sys::Win32::Foundation::CloseHandle(job);
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Windows Job Object setup failed: {error}");
        }
        if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
            let error = std::io::Error::last_os_error();
            windows_sys::Win32::Foundation::CloseHandle(job);
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32)
            {
                if let Err(error) = resume_process(child.id()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                return Ok(0);
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Windows Job Object assignment failed: {error}");
        }
        if let Err(error) = resume_process(child.id()) {
            windows_sys::Win32::Foundation::CloseHandle(job);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(job as usize)
    }
}

#[cfg(windows)]
fn resume_process(process_id: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            anyhow::bail!(
                "failed to enumerate suspended process threads: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut has_entry = Thread32First(snapshot, &mut entry) != 0;
        let mut resumed = false;
        while has_entry {
            if entry.th32OwnerProcessID == process_id {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(snapshot);
                    anyhow::bail!("failed to open suspended process thread: {error}");
                }
                let result = ResumeThread(thread);
                CloseHandle(thread);
                if result == u32::MAX {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(snapshot);
                    anyhow::bail!("failed to resume service process: {error}");
                }
                resumed = true;
                break;
            }
            has_entry = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if !resumed {
            anyhow::bail!("suspended service process had no discoverable primary thread");
        }
    }
    Ok(())
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    service: &str,
    stderr: bool,
    reader: R,
    tx: SyncSender<LogEvent>,
    system_tx: Sender<LogEvent>,
    stop: Arc<AtomicBool>,
    ui: bool,
) -> JoinHandle<()> {
    let service = service.to_string();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut pending = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut drop_reported = false;
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let read = match reader.read(&mut chunk) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if read == 0 {
                if !pending.is_empty() {
                    forward_log_line(
                        &service,
                        stderr,
                        &pending,
                        &tx,
                        &system_tx,
                        ui,
                        &mut drop_reported,
                    );
                }
                break;
            }
            pending.extend_from_slice(&chunk[..read]);
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let mut line = pending.drain(..=newline).collect::<Vec<_>>();
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                forward_log_line(
                    &service,
                    stderr,
                    &line,
                    &tx,
                    &system_tx,
                    ui,
                    &mut drop_reported,
                );
            }
            const MAX_PENDING_LINE: usize = 64 * 1024;
            while pending.len() >= MAX_PENDING_LINE {
                let mut bounded = pending.drain(..MAX_PENDING_LINE).collect::<Vec<_>>();
                bounded.extend_from_slice(b" [aster: long line continued]");
                forward_log_line(
                    &service,
                    stderr,
                    &bounded,
                    &tx,
                    &system_tx,
                    ui,
                    &mut drop_reported,
                );
            }
        }
    })
}

fn forward_log_line(
    service: &str,
    stderr: bool,
    bytes: &[u8],
    tx: &SyncSender<LogEvent>,
    system_tx: &Sender<LogEvent>,
    ui: bool,
    drop_reported: &mut bool,
) {
    let line = String::from_utf8_lossy(bytes).into_owned();
    if !ui {
        let stream = if stderr { "!" } else { "|" };
        println!("[{service}] {stream} {line}");
        return;
    }
    let event = LogEvent {
        service: service.to_string(),
        line,
        stderr,
    };
    match tx.try_send(event) {
        Ok(()) => *drop_reported = false,
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            if !*drop_reported {
                let _ = system_tx.send(LogEvent {
                    service: service.to_string(),
                    line: "[aster] service output omitted while the UI was busy".to_string(),
                    stderr: true,
                });
                *drop_reported = true;
            }
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
    }
}
