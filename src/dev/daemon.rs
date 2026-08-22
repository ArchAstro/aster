//! Per-effective-user broker for the existing `aster services up` supervisor.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_GROUP: &str = "__aster_default__";
const SERVE_ENV: &str = "ASTER_INTERNAL_DAEMON_SERVE";
const READY_SOCKET_ENV: &str = "ASTER_INTERNAL_DAEMON_READY_SOCKET";
const READY_ID_ENV: &str = "ASTER_INTERNAL_DAEMON_BUNDLE_ID";
const READY_GROUP_ENV: &str = "ASTER_INTERNAL_DAEMON_GROUP";
const READY_DISPLAY_GROUP_ENV: &str = "ASTER_INTERNAL_DAEMON_DISPLAY_GROUP";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonErrorCode {
    UnsupportedPlatform,
    UnsupportedProtocol,
    InvalidRequest,
    InvalidPath,
    InsecureRuntimeDirectory,
    EndpointCollision,
    DaemonUnavailable,
    StartupFailed,
    NotFound,
    Busy,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonError {
    pub code: DaemonErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for DaemonError {}

pub type DaemonResult<T> = Result<T, DaemonError>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleState {
    Starting,
    Running,
    Stopping,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BundleDescriptor {
    pub bundle_id: String,
    pub workspace: PathBuf,
    pub group: String,
    pub display_group: Option<String>,
    pub supervisor_pid: u32,
    pub state: BundleState,
    pub services: Vec<String>,
    pub ports: BTreeMap<String, u16>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStatus {
    Started,
    AlreadyRunning,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LaunchResult {
    pub status: LaunchStatus,
    pub bundle: BundleDescriptor,
}

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub workspace: PathBuf,
    pub group: Option<String>,
    pub watch: bool,
    pub use_cache: bool,
    pub executable: PathBuf,
}

impl LaunchOptions {
    pub fn new(workspace: impl Into<PathBuf>) -> DaemonResult<Self> {
        let executable = std::env::current_exe().map_err(|error| {
            daemon_error(
                DaemonErrorCode::InvalidPath,
                format!("failed to locate current Aster executable: {error}"),
            )
        })?;
        Ok(Self {
            workspace: workspace.into(),
            group: None,
            watch: true,
            use_cache: true,
            executable,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireRequest {
    version: u16,
    #[serde(flatten)]
    operation: Operation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
enum Operation {
    Ping,
    Launch {
        workspace: PathBuf,
        group: Option<String>,
        watch: bool,
        use_cache: bool,
        executable: PathBuf,
    },
    ListWorkspace {
        workspace: PathBuf,
    },
    StopWorkspace {
        workspace: PathBuf,
        group: Option<String>,
    },
    StopAll,
}

#[derive(Debug, Deserialize, Serialize)]
struct WireResponse {
    version: u16,
    #[serde(flatten)]
    result: WireResult,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResult {
    Ok { value: ResponseValue },
    Error { error: DaemonError },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum ResponseValue {
    Pong { daemon_pid: u32 },
    Launch(LaunchResult),
    Bundles(Vec<BundleDescriptor>),
    Stopped(Vec<BundleDescriptor>),
}

fn daemon_error(code: DaemonErrorCode, message: impl Into<String>) -> DaemonError {
    DaemonError {
        code,
        message: message.into(),
        diagnostics: None,
    }
}

fn normalize_group(group: Option<&str>) -> DaemonResult<(String, Option<String>)> {
    match group {
        None => Ok((DEFAULT_GROUP.to_string(), None)),
        Some(group) => {
            let trimmed = group.trim();
            if trimmed.is_empty() || trimmed.contains('\0') || trimmed == DEFAULT_GROUP {
                return Err(daemon_error(
                    DaemonErrorCode::InvalidRequest,
                    "service group is empty or reserved for Aster's default bundle",
                ));
            }
            Ok((trimmed.to_string(), Some(trimmed.to_string())))
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use fs2::FileExt;
    use std::collections::HashMap;
    use std::fs::{self, File, OpenOptions};
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const SOCKET_NAME: &str = "daemon.sock";
    const LOCK_NAME: &str = "daemon.lock";
    const PID_NAME: &str = "daemon.pid";
    const LOG_NAME: &str = "daemon.log";
    const READY_NAME: &str = "ready.sock";
    const START_TIMEOUT: Duration = Duration::from_secs(20);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const IDLE_GRACE: Duration = Duration::from_millis(500);
    const STOP_GRACE: Duration = Duration::from_secs(5);

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct BundleKey {
        workspace: PathBuf,
        group: String,
    }

    struct ManagedBundle {
        descriptor: BundleDescriptor,
        child: Child,
        startup_log: PathBuf,
        ready_deadline: Instant,
        waiters: Vec<(UnixStream, LaunchStatus)>,
    }

    #[derive(Debug, Deserialize)]
    struct ReadyRecord {
        version: u16,
        bundle_id: String,
        workspace: PathBuf,
        group: String,
        display_group: Option<String>,
        supervisor_pid: u32,
        services: Vec<String>,
        ports: BTreeMap<String, u16>,
    }

    pub(super) fn is_serve_invocation() -> bool {
        std::env::var_os(SERVE_ENV).is_some()
    }

    pub(super) fn serve_from_environment() -> DaemonResult<()> {
        std::env::remove_var(SERVE_ENV);
        let paths = RuntimePaths::secure()?;
        let lock = open_regular(&paths.lock, true)?;
        lock.lock_exclusive()
            .map_err(internal("failed to acquire daemon lock"))?;
        if endpoint_kind(&paths.socket)?.is_some() {
            if ping_existing(&paths.socket).is_ok() {
                return Err(daemon_error(
                    DaemonErrorCode::Busy,
                    "an Aster daemon is already running",
                ));
            }
            remove_verified_socket(&paths.socket)?;
        }
        remove_if_socket(&paths.ready)?;
        let listener =
            UnixListener::bind(&paths.socket).map_err(internal("failed to bind daemon socket"))?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .map_err(internal("failed to secure daemon socket"))?;
        listener
            .set_nonblocking(true)
            .map_err(internal("failed to configure daemon socket"))?;
        let ready = UnixDatagram::bind(&paths.ready)
            .map_err(internal("failed to bind readiness socket"))?;
        fs::set_permissions(&paths.ready, fs::Permissions::from_mode(0o600))
            .map_err(internal("failed to secure readiness socket"))?;
        ready
            .set_nonblocking(true)
            .map_err(internal("failed to configure readiness socket"))?;
        write_pid(&paths.pid)?;
        let result = serve_loop(listener, ready, &paths);
        let _ = fs::remove_file(&paths.socket);
        let _ = fs::remove_file(&paths.ready);
        let _ = fs::remove_file(&paths.pid);
        result
    }

    pub(super) fn ping() -> DaemonResult<u32> {
        let paths = RuntimePaths::secure()?;
        let executable =
            std::env::current_exe().map_err(internal("failed to locate Aster executable"))?;
        ensure_daemon(&paths, &executable)?;
        match request(&paths.socket, Operation::Ping)? {
            ResponseValue::Pong { daemon_pid } => Ok(daemon_pid),
            _ => Err(protocol_mismatch()),
        }
    }

    pub(super) fn launch(options: LaunchOptions) -> DaemonResult<LaunchResult> {
        let paths = RuntimePaths::secure()?;
        ensure_daemon(&paths, &options.executable)?;
        let operation = Operation::Launch {
            workspace: options.workspace,
            group: options.group,
            watch: options.watch,
            use_cache: options.use_cache,
            executable: options.executable,
        };
        match request(&paths.socket, operation)? {
            ResponseValue::Launch(result) => Ok(result),
            _ => Err(protocol_mismatch()),
        }
    }

    pub(super) fn list_workspace(workspace: &Path) -> DaemonResult<Vec<BundleDescriptor>> {
        let paths = RuntimePaths::secure()?;
        let executable =
            std::env::current_exe().map_err(internal("failed to locate Aster executable"))?;
        ensure_daemon(&paths, &executable)?;
        match request(
            &paths.socket,
            Operation::ListWorkspace {
                workspace: workspace.to_path_buf(),
            },
        )? {
            ResponseValue::Bundles(value) => Ok(value),
            _ => Err(protocol_mismatch()),
        }
    }

    pub(super) fn stop_workspace(
        workspace: &Path,
        group: Option<&str>,
    ) -> DaemonResult<Vec<BundleDescriptor>> {
        let paths = RuntimePaths::secure()?;
        let executable =
            std::env::current_exe().map_err(internal("failed to locate Aster executable"))?;
        ensure_daemon(&paths, &executable)?;
        match request(
            &paths.socket,
            Operation::StopWorkspace {
                workspace: workspace.to_path_buf(),
                group: group.map(str::to_string),
            },
        )? {
            ResponseValue::Stopped(value) => Ok(value),
            _ => Err(protocol_mismatch()),
        }
    }

    pub(super) fn stop_all() -> DaemonResult<Vec<BundleDescriptor>> {
        let paths = RuntimePaths::secure()?;
        let executable =
            std::env::current_exe().map_err(internal("failed to locate Aster executable"))?;
        ensure_daemon(&paths, &executable)?;
        match request(&paths.socket, Operation::StopAll)? {
            ResponseValue::Stopped(value) => Ok(value),
            _ => Err(protocol_mismatch()),
        }
    }

    pub(super) fn register_ready(
        workspace: &Path,
        services: Vec<String>,
        ports: BTreeMap<String, u16>,
    ) -> DaemonResult<()> {
        let Some(socket) = std::env::var_os(READY_SOCKET_ENV) else {
            return Ok(());
        };
        let bundle_id = std::env::var(READY_ID_ENV).map_err(|_| {
            daemon_error(DaemonErrorCode::InvalidRequest, "missing daemon bundle id")
        })?;
        let group = std::env::var(READY_GROUP_ENV)
            .map_err(|_| daemon_error(DaemonErrorCode::InvalidRequest, "missing daemon group"))?;
        let display_group = std::env::var(READY_DISPLAY_GROUP_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        let workspace = canonical_directory(workspace, "workspace")?;
        let record = serde_json::json!({
            "version": PROTOCOL_VERSION, "bundle_id": bundle_id, "workspace": workspace,
            "group": group, "display_group": display_group, "supervisor_pid": std::process::id(),
            "services": services, "ports": ports,
        });
        let datagram =
            UnixDatagram::unbound().map_err(internal("failed to create readiness channel"))?;
        datagram
            .send_to(record.to_string().as_bytes(), socket)
            .map_err(internal("failed to register supervisor readiness"))?;
        Ok(())
    }

    pub(super) struct RuntimePaths {
        directory: PathBuf,
        socket: PathBuf,
        lock: PathBuf,
        pid: PathBuf,
        log: PathBuf,
        ready: PathBuf,
    }

    impl RuntimePaths {
        pub(super) fn secure() -> DaemonResult<Self> {
            static RUNTIME_CREATION: OnceLock<Mutex<()>> = OnceLock::new();
            let _creation_guard = RUNTIME_CREATION
                .get_or_init(|| Mutex::new(()))
                .lock()
                .map_err(|_| {
                    daemon_error(
                        DaemonErrorCode::Internal,
                        "daemon runtime creation lock is poisoned",
                    )
                })?;
            let directory = runtime_directory();
            match fs::symlink_metadata(&directory) {
                Ok(metadata) => validate_runtime_metadata(&directory, &metadata)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match fs::create_dir(&directory) {
                        Ok(()) => {
                            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                                .map_err(internal("failed to secure daemon runtime directory"))?;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => {
                            return Err(daemon_error(
                                DaemonErrorCode::Internal,
                                format!("failed to create daemon runtime directory: {error}"),
                            ));
                        }
                    }
                    validate_runtime_metadata(
                        &directory,
                        &fs::symlink_metadata(&directory)
                            .map_err(internal("failed to inspect daemon runtime directory"))?,
                    )?;
                }
                Err(error) => {
                    return Err(daemon_error(
                        DaemonErrorCode::InsecureRuntimeDirectory,
                        format!("failed to inspect {}: {error}", directory.display()),
                    ))
                }
            }
            Ok(Self {
                socket: directory.join(SOCKET_NAME),
                lock: directory.join(LOCK_NAME),
                pid: directory.join(PID_NAME),
                log: directory.join(LOG_NAME),
                ready: directory.join(READY_NAME),
                directory,
            })
        }
    }

    fn runtime_directory() -> PathBuf {
        if let Some(path) = std::env::var_os("ASTER_DAEMON_RUNTIME_DIR") {
            return PathBuf::from(path);
        }
        let uid = unsafe { libc::geteuid() };
        std::env::temp_dir().join(format!("aster-daemon-v1-{uid}"))
    }

    fn validate_runtime_metadata(path: &Path, metadata: &fs::Metadata) -> DaemonResult<()> {
        let uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(daemon_error(DaemonErrorCode::InsecureRuntimeDirectory,
                format!("daemon runtime directory {} must be owned by effective uid {uid} with mode 0700", path.display())));
        }
        Ok(())
    }

    fn open_regular(path: &Path, create: bool) -> DaemonResult<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        if create {
            options.create(true);
        }
        let file = options
            .open(path)
            .map_err(internal("failed to open daemon runtime file"))?;
        let metadata = file
            .metadata()
            .map_err(internal("failed to inspect daemon runtime file"))?;
        if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(daemon_error(
                DaemonErrorCode::EndpointCollision,
                format!("unsafe daemon runtime file {}", path.display()),
            ));
        }
        Ok(file)
    }

    fn ensure_daemon(paths: &RuntimePaths, daemon_executable: &Path) -> DaemonResult<()> {
        if ping_existing(&paths.socket).is_ok() {
            return Ok(());
        }
        let lock = open_regular(&paths.lock, true)?;
        lock.lock_exclusive()
            .map_err(internal("failed to acquire daemon startup lock"))?;
        if ping_existing(&paths.socket).is_ok() {
            return Ok(());
        }
        if let Some(is_socket) = endpoint_kind(&paths.socket)? {
            if !is_socket {
                return Err(daemon_error(
                    DaemonErrorCode::EndpointCollision,
                    format!("daemon endpoint {} is not a socket", paths.socket.display()),
                ));
            }
            remove_verified_socket(&paths.socket)?;
        }
        spawn_daemon(paths, daemon_executable)?;
        FileExt::unlock(&lock).map_err(internal("failed to release daemon startup lock"))?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match ping_existing(&paths.socket) {
                Ok(_) => return Ok(()),
                Err(_error) if Instant::now() < deadline => {
                    if let Ok(Some(status)) = daemon_pid_status(&paths.pid) {
                        return Err(daemon_error(
                            DaemonErrorCode::DaemonUnavailable,
                            format!("daemon exited during startup: {status}"),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(daemon_error(
                        DaemonErrorCode::DaemonUnavailable,
                        format!("daemon did not become ready: {error}"),
                    ))
                }
            }
        }
    }

    fn spawn_daemon(paths: &RuntimePaths, executable: &Path) -> DaemonResult<()> {
        let executable = canonical_file(executable, "Aster executable")?;
        let mut log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&paths.log)
            .map_err(internal("failed to open daemon diagnostic log"))?;
        writeln!(log, "starting Aster daemon from {}", executable.display())
            .map_err(internal("failed to write daemon log"))?;
        let stderr = log
            .try_clone()
            .map_err(internal("failed to clone daemon log"))?;
        let mut command = Command::new(executable);
        command
            .env(SERVE_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
            .spawn()
            .map_err(internal("failed to spawn Aster daemon"))?;
        Ok(())
    }

    fn request(socket: &Path, operation: Operation) -> DaemonResult<ResponseValue> {
        let mut stream =
            UnixStream::connect(socket).map_err(internal("failed to connect to Aster daemon"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(internal("failed to configure daemon connection"))?;
        serde_json::to_writer(
            &mut stream,
            &WireRequest {
                version: PROTOCOL_VERSION,
                operation,
            },
        )
        .map_err(|error| {
            daemon_error(
                DaemonErrorCode::Internal,
                format!("failed to encode daemon request: {error}"),
            )
        })?;
        writeln!(stream).map_err(internal("failed to send daemon request"))?;
        let response: WireResponse =
            serde_json::from_reader(BufReader::new(stream)).map_err(|error| {
                daemon_error(
                    DaemonErrorCode::Internal,
                    format!("failed to decode daemon response: {error}"),
                )
            })?;
        if response.version != PROTOCOL_VERSION {
            return Err(protocol_mismatch());
        }
        match response.result {
            WireResult::Ok { value } => Ok(value),
            WireResult::Error { error } => Err(error),
        }
    }

    fn ping_existing(socket: &Path) -> DaemonResult<u32> {
        match request(socket, Operation::Ping)? {
            ResponseValue::Pong { daemon_pid } => Ok(daemon_pid),
            _ => Err(protocol_mismatch()),
        }
    }

    fn serve_loop(
        listener: UnixListener,
        ready: UnixDatagram,
        paths: &RuntimePaths,
    ) -> DaemonResult<()> {
        let mut bundles: HashMap<BundleKey, ManagedBundle> = HashMap::new();
        let mut idle_since: Option<Instant> = None;
        let mut stopping_all = false;
        loop {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Err(error) = handle_connection(
                            stream
                                .try_clone()
                                .map_err(internal("failed to clone daemon connection"))?,
                            paths,
                            &mut bundles,
                            &mut stopping_all,
                        ) {
                            let _ = write_error(&mut stream, error);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        return Err(daemon_error(
                            DaemonErrorCode::Internal,
                            format!("failed to accept daemon connection: {error}"),
                        ))
                    }
                }
            }
            drain_readiness(&ready, &mut bundles)?;
            reap_and_timeout(&mut bundles)?;
            if stopping_all && bundles.is_empty() {
                return Ok(());
            }
            if bundles.is_empty() {
                let since = idle_since.get_or_insert_with(Instant::now);
                if *since + IDLE_GRACE <= Instant::now() {
                    return Ok(());
                }
            } else {
                idle_since = None;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn handle_connection(
        mut stream: UnixStream,
        paths: &RuntimePaths,
        bundles: &mut HashMap<BundleKey, ManagedBundle>,
        stopping_all: &mut bool,
    ) -> DaemonResult<()> {
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut line = String::new();
        if let Err(error) = BufReader::new(
            stream
                .try_clone()
                .map_err(internal("failed to read daemon request"))?,
        )
        .read_line(&mut line)
        {
            return write_error(
                &mut stream,
                daemon_error(
                    DaemonErrorCode::InvalidRequest,
                    format!("failed to read request: {error}"),
                ),
            );
        }
        let request: WireRequest = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                return write_error(
                    &mut stream,
                    daemon_error(
                        DaemonErrorCode::InvalidRequest,
                        format!("invalid JSON request: {error}"),
                    ),
                )
            }
        };
        if request.version != PROTOCOL_VERSION {
            return write_error(&mut stream, protocol_mismatch());
        }
        match request.operation {
            Operation::Ping => write_value(
                &mut stream,
                ResponseValue::Pong {
                    daemon_pid: std::process::id(),
                },
            ),
            Operation::Launch {
                workspace,
                group,
                watch,
                use_cache,
                executable,
            } => {
                if *stopping_all {
                    return write_error(
                        &mut stream,
                        daemon_error(DaemonErrorCode::Busy, "daemon is stopping all bundles"),
                    );
                }
                let workspace = canonical_directory(&workspace, "workspace")?;
                let executable = canonical_file(&executable, "Aster executable")?;
                let (group, display_group) = normalize_group(group.as_deref())?;
                let key = BundleKey {
                    workspace: workspace.clone(),
                    group: group.clone(),
                };
                if let Some(bundle) = bundles.get_mut(&key) {
                    if bundle.descriptor.state == BundleState::Running {
                        return write_value(
                            &mut stream,
                            ResponseValue::Launch(LaunchResult {
                                status: LaunchStatus::AlreadyRunning,
                                bundle: bundle.descriptor.clone(),
                            }),
                        );
                    }
                    bundle.waiters.push((stream, LaunchStatus::AlreadyRunning));
                    return Ok(());
                }
                let bundle_id = bundle_id(&workspace, &group);
                let startup_log = paths.directory.join(format!("supervisor-{bundle_id}.log"));
                let spawn = SupervisorSpawn {
                    executable: &executable,
                    workspace: &workspace,
                    display_group: display_group.as_deref(),
                    watch,
                    use_cache,
                    ready_socket: &paths.ready,
                    bundle_id: &bundle_id,
                    group: &group,
                    log_path: &startup_log,
                };
                let child = spawn_supervisor(&spawn)?;
                let descriptor = BundleDescriptor {
                    bundle_id,
                    workspace,
                    group,
                    display_group,
                    supervisor_pid: child.id(),
                    state: BundleState::Starting,
                    services: Vec::new(),
                    ports: BTreeMap::new(),
                };
                bundles.insert(
                    key,
                    ManagedBundle {
                        descriptor,
                        child,
                        startup_log,
                        ready_deadline: Instant::now() + START_TIMEOUT,
                        waiters: vec![(stream, LaunchStatus::Started)],
                    },
                );
                Ok(())
            }
            Operation::ListWorkspace { workspace } => {
                let workspace = canonical_directory(&workspace, "workspace")?;
                let values = bundles
                    .values()
                    .filter(|bundle| bundle.descriptor.workspace == workspace)
                    .map(|bundle| bundle.descriptor.clone())
                    .collect();
                write_value(&mut stream, ResponseValue::Bundles(values))
            }
            Operation::StopWorkspace { workspace, group } => {
                let workspace = canonical_directory(&workspace, "workspace")?;
                let normalized = group
                    .as_deref()
                    .map(|value| normalize_group(Some(value)).map(|pair| pair.0))
                    .transpose()?;
                let keys: Vec<_> = bundles
                    .keys()
                    .filter(|key| {
                        key.workspace == workspace
                            && normalized.as_ref().is_none_or(|group| &key.group == group)
                    })
                    .cloned()
                    .collect();
                let stopped = stop_keys(bundles, keys);
                write_value(&mut stream, ResponseValue::Stopped(stopped))
            }
            Operation::StopAll => {
                *stopping_all = true;
                let keys = bundles.keys().cloned().collect();
                let stopped = stop_keys(bundles, keys);
                write_value(&mut stream, ResponseValue::Stopped(stopped))
            }
        }
    }

    struct SupervisorSpawn<'a> {
        executable: &'a Path,
        workspace: &'a Path,
        display_group: Option<&'a str>,
        watch: bool,
        use_cache: bool,
        ready_socket: &'a Path,
        bundle_id: &'a str,
        group: &'a str,
        log_path: &'a Path,
    }

    fn spawn_supervisor(options: &SupervisorSpawn<'_>) -> DaemonResult<Child> {
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(options.log_path)
            .map_err(internal("failed to open supervisor startup log"))?;
        let stderr = log
            .try_clone()
            .map_err(internal("failed to clone supervisor startup log"))?;
        let mut command = Command::new(options.executable);
        command
            .current_dir(options.workspace)
            .args(["services", "up"]);
        if let Some(group) = options.display_group {
            command.arg(group);
        }
        command.arg("--no-ui");
        if !options.watch {
            command.arg("--no-watch");
        }
        if !options.use_cache {
            command.arg("--no-cache");
        }
        command
            .env(READY_SOCKET_ENV, options.ready_socket)
            .env(READY_ID_ENV, options.bundle_id)
            .env(READY_GROUP_ENV, options.group)
            .env(READY_DISPLAY_GROUP_ENV, options.display_group.unwrap_or(""))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .process_group(0);
        command
            .spawn()
            .map_err(internal("failed to spawn service supervisor"))
    }

    fn drain_readiness(
        socket: &UnixDatagram,
        bundles: &mut HashMap<BundleKey, ManagedBundle>,
    ) -> DaemonResult<()> {
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let size = match socket.recv(&mut buffer) {
                Ok(size) => size,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    return Err(daemon_error(
                        DaemonErrorCode::Internal,
                        format!("failed to read readiness record: {error}"),
                    ))
                }
            };
            let record: ReadyRecord = match serde_json::from_slice(&buffer[..size]) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if record.version != PROTOCOL_VERSION {
                continue;
            }
            let key = BundleKey {
                workspace: record.workspace.clone(),
                group: record.group.clone(),
            };
            let Some(bundle) = bundles.get_mut(&key) else {
                continue;
            };
            if bundle.descriptor.bundle_id != record.bundle_id
                || bundle.child.id() != record.supervisor_pid
                || bundle.descriptor.display_group != record.display_group
            {
                continue;
            }
            bundle.descriptor.state = BundleState::Running;
            bundle.descriptor.services = record.services;
            bundle.descriptor.ports = record.ports;
            let descriptor = bundle.descriptor.clone();
            for (mut waiter, status) in std::mem::take(&mut bundle.waiters) {
                let _ = write_value(
                    &mut waiter,
                    ResponseValue::Launch(LaunchResult {
                        status,
                        bundle: descriptor.clone(),
                    }),
                );
            }
        }
        Ok(())
    }

    fn reap_and_timeout(bundles: &mut HashMap<BundleKey, ManagedBundle>) -> DaemonResult<()> {
        let mut remove = Vec::new();
        for (key, bundle) in bundles.iter_mut() {
            let exited = bundle
                .child
                .try_wait()
                .map_err(internal("failed to reap service supervisor"))?;
            if exited.is_some()
                || (bundle.descriptor.state == BundleState::Starting
                    && Instant::now() >= bundle.ready_deadline)
            {
                if exited.is_none() {
                    let _ = terminate_child(&mut bundle.child);
                }
                let diagnostics = tail_file(&bundle.startup_log, 16 * 1024);
                let message = exited.map_or_else(
                    || "service supervisor readiness timed out".to_string(),
                    |status| format!("service supervisor exited before readiness: {status}"),
                );
                let error = DaemonError {
                    code: DaemonErrorCode::StartupFailed,
                    message,
                    diagnostics,
                };
                for (mut waiter, _) in std::mem::take(&mut bundle.waiters) {
                    let _ = write_error(&mut waiter, error.clone());
                }
                remove.push(key.clone());
            }
        }
        for key in remove {
            bundles.remove(&key);
        }
        Ok(())
    }

    fn stop_keys(
        bundles: &mut HashMap<BundleKey, ManagedBundle>,
        keys: Vec<BundleKey>,
    ) -> Vec<BundleDescriptor> {
        let mut stopped = Vec::new();
        for key in keys {
            if let Some(mut bundle) = bundles.remove(&key) {
                bundle.descriptor.state = BundleState::Stopping;
                let _ = terminate_child(&mut bundle.child);
                for (mut waiter, _) in bundle.waiters {
                    let _ = write_error(
                        &mut waiter,
                        daemon_error(
                            DaemonErrorCode::StartupFailed,
                            "service supervisor was stopped during startup",
                        ),
                    );
                }
                stopped.push(bundle.descriptor);
            }
        }
        stopped
    }

    fn terminate_child(child: &mut Child) -> std::io::Result<()> {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        child.wait().map(|_| ())
    }

    fn write_value(stream: &mut UnixStream, value: ResponseValue) -> DaemonResult<()> {
        write_response(stream, WireResult::Ok { value })
    }
    fn write_error(stream: &mut UnixStream, error: DaemonError) -> DaemonResult<()> {
        write_response(stream, WireResult::Error { error })
    }
    fn write_response(stream: &mut UnixStream, result: WireResult) -> DaemonResult<()> {
        serde_json::to_writer(
            &mut *stream,
            &WireResponse {
                version: PROTOCOL_VERSION,
                result,
            },
        )
        .map_err(|error| {
            daemon_error(
                DaemonErrorCode::Internal,
                format!("failed to encode daemon response: {error}"),
            )
        })?;
        writeln!(stream).map_err(internal("failed to write daemon response"))
    }

    fn canonical_directory(path: &Path, label: &str) -> DaemonResult<PathBuf> {
        let path = path.canonicalize().map_err(|error| {
            daemon_error(
                DaemonErrorCode::InvalidPath,
                format!("failed to canonicalize {label} {}: {error}", path.display()),
            )
        })?;
        if !path.is_dir() {
            return Err(daemon_error(
                DaemonErrorCode::InvalidPath,
                format!("{label} {} is not a directory", path.display()),
            ));
        }
        Ok(path)
    }
    fn canonical_file(path: &Path, label: &str) -> DaemonResult<PathBuf> {
        let path = path.canonicalize().map_err(|error| {
            daemon_error(
                DaemonErrorCode::InvalidPath,
                format!("failed to canonicalize {label} {}: {error}", path.display()),
            )
        })?;
        if !path.is_file() {
            return Err(daemon_error(
                DaemonErrorCode::InvalidPath,
                format!("{label} {} is not a file", path.display()),
            ));
        }
        Ok(path)
    }

    pub(super) fn bundle_id(workspace: &Path, group: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(workspace.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(group.as_bytes());
        let digest = hash.finalize();
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    fn endpoint_kind(path: &Path) -> DaemonResult<Option<bool>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(metadata.file_type().is_socket())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(daemon_error(
                DaemonErrorCode::EndpointCollision,
                format!("failed to inspect daemon endpoint: {error}"),
            )),
        }
    }
    fn remove_verified_socket(path: &Path) -> DaemonResult<()> {
        if endpoint_kind(path)? != Some(true) {
            return Err(daemon_error(
                DaemonErrorCode::EndpointCollision,
                "refusing to remove non-socket daemon endpoint",
            ));
        }
        fs::remove_file(path).map_err(internal("failed to remove stale daemon socket"))
    }
    fn remove_if_socket(path: &Path) -> DaemonResult<()> {
        match endpoint_kind(path)? {
            None => Ok(()),
            Some(true) => {
                fs::remove_file(path).map_err(internal("failed to remove stale readiness socket"))
            }
            Some(false) => Err(daemon_error(
                DaemonErrorCode::EndpointCollision,
                format!("{} is not a socket", path.display()),
            )),
        }
    }
    fn write_pid(path: &Path) -> DaemonResult<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(internal("failed to create daemon PID file"))?;
        writeln!(file, "{}", std::process::id())
            .map_err(internal("failed to write daemon PID file"))
    }
    fn daemon_pid_status(path: &Path) -> std::io::Result<Option<String>> {
        let mut value = String::new();
        match File::open(path) {
            Ok(mut file) => {
                file.read_to_string(&mut value)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let Ok(pid) = value.trim().parse::<i32>() else {
            return Ok(None);
        };
        if unsafe { libc::kill(pid, 0) } == -1 {
            Ok(Some(format!("pid {pid}")))
        } else {
            Ok(None)
        }
    }
    fn tail_file(path: &Path, limit: u64) -> Option<String> {
        let mut file = File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(limit)))
            .ok()?;
        let mut value = String::new();
        file.read_to_string(&mut value).ok()?;
        Some(value)
    }
    fn internal(context: &'static str) -> impl FnOnce(std::io::Error) -> DaemonError {
        move |error| daemon_error(DaemonErrorCode::Internal, format!("{context}: {error}"))
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;
    fn unsupported<T>() -> DaemonResult<T> {
        Err(daemon_error(
            DaemonErrorCode::UnsupportedPlatform,
            "the Aster daemon is not supported on Windows",
        ))
    }
    pub(super) fn is_serve_invocation() -> bool {
        false
    }
    pub(super) fn serve_from_environment() -> DaemonResult<()> {
        unsupported()
    }
    pub(super) fn ping() -> DaemonResult<u32> {
        unsupported()
    }
    pub(super) fn launch(_: LaunchOptions) -> DaemonResult<LaunchResult> {
        unsupported()
    }
    pub(super) fn list_workspace(_: &Path) -> DaemonResult<Vec<BundleDescriptor>> {
        unsupported()
    }
    pub(super) fn stop_workspace(_: &Path, _: Option<&str>) -> DaemonResult<Vec<BundleDescriptor>> {
        unsupported()
    }
    pub(super) fn stop_all() -> DaemonResult<Vec<BundleDescriptor>> {
        unsupported()
    }
    pub(super) fn register_ready(
        _: &Path,
        _: Vec<String>,
        _: BTreeMap<String, u16>,
    ) -> DaemonResult<()> {
        Ok(())
    }
}

pub fn ping_daemon() -> DaemonResult<u32> {
    platform::ping()
}
pub fn launch_bundle(options: LaunchOptions) -> DaemonResult<LaunchResult> {
    platform::launch(options)
}
pub fn list_workspace_bundles(workspace: &Path) -> DaemonResult<Vec<BundleDescriptor>> {
    platform::list_workspace(workspace)
}
pub fn stop_workspace_bundles(
    workspace: &Path,
    group: Option<&str>,
) -> DaemonResult<Vec<BundleDescriptor>> {
    platform::stop_workspace(workspace, group)
}
pub fn stop_all_bundles() -> DaemonResult<Vec<BundleDescriptor>> {
    platform::stop_all()
}

#[doc(hidden)]
pub fn is_internal_serve_invocation() -> bool {
    platform::is_serve_invocation()
}
#[doc(hidden)]
pub fn serve_from_environment() -> DaemonResult<()> {
    platform::serve_from_environment()
}

pub fn register_supervisor_ready(
    workspace: &Path,
    services: Vec<String>,
    ports: BTreeMap<String, u16>,
) -> DaemonResult<()> {
    platform::register_ready(workspace, services, ports)
}

fn protocol_mismatch() -> DaemonError {
    daemon_error(
        DaemonErrorCode::UnsupportedProtocol,
        format!("daemon protocol version {PROTOCOL_VERSION} is required"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn omitted_group_has_stable_sentinel() {
        assert_eq!(
            normalize_group(None).unwrap(),
            (DEFAULT_GROUP.to_string(), None)
        );
    }
    #[test]
    fn protocol_rejects_unknown_version() {
        let decoded: WireRequest =
            serde_json::from_str(r#"{"version":99,"operation":"ping"}"#).unwrap();
        assert_ne!(decoded.version, PROTOCOL_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directory_rejects_insecure_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("ASTER_DAEMON_RUNTIME_DIR", &runtime);
        let error = platform::RuntimePaths::secure().err().unwrap();
        std::env::remove_var("ASTER_DAEMON_RUNTIME_DIR");
        assert_eq!(error.code, DaemonErrorCode::InsecureRuntimeDirectory);
    }

    #[cfg(unix)]
    #[test]
    fn canonical_bundle_id_is_stable_and_group_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        assert_eq!(
            platform::bundle_id(&canonical, DEFAULT_GROUP),
            platform::bundle_id(&canonical, DEFAULT_GROUP)
        );
        assert_ne!(
            platform::bundle_id(&canonical, DEFAULT_GROUP),
            platform::bundle_id(&canonical, "api")
        );
    }
}
