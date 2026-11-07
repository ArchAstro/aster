use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// OS-backed leases held for the lifetime of one service supervisor.
///
/// Dropping the files releases every lock, including when the process is
/// terminated without running application cleanup.
pub struct PortLease {
    #[allow(dead_code)]
    files: Vec<File>,
    manifest_path: PathBuf,
}

impl Drop for PortLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.manifest_path);
    }
}

pub(crate) struct PortAllocator {
    directory: PathBuf,
    _allocation_lock: File,
    files: Vec<File>,
    ports: HashSet<u16>,
    named_ports: BTreeMap<String, u16>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AllocationManifest {
    version: u8,
    supervisor_pid: u32,
    workspace_root: String,
    ports: BTreeMap<String, u16>,
    #[serde(default)]
    services: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PortAllocationStatus {
    Active,
    Orphaned,
}

#[derive(Debug)]
pub(crate) struct WorkspacePortAllocation {
    pub supervisor_pid: u32,
    pub status: PortAllocationStatus,
    pub ports: BTreeMap<String, u16>,
    pub services: BTreeMap<String, Option<String>>,
}

impl PortAllocator {
    pub(crate) fn lock() -> Result<Self> {
        let directory = lease_directory();
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "failed to create port lease directory {}",
                directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(
                || {
                    format!(
                        "failed to secure port lease directory {}",
                        directory.display()
                    )
                },
            )?;
        }
        let allocation_lock = open_lock(&directory.join("allocation.lock"))?;
        allocation_lock
            .lock_exclusive()
            .context("failed to acquire the Aster port allocator lock")?;
        Ok(Self {
            directory,
            _allocation_lock: allocation_lock,
            files: Vec::new(),
            ports: HashSet::new(),
            named_ports: BTreeMap::new(),
        })
    }

    /// Try one complete bundle. A failed candidate releases all of its locks.
    pub(crate) fn try_bundle(
        &mut self,
        workspace_root: &Path,
        ports: &BTreeMap<String, u16>,
    ) -> Result<bool> {
        let mut values = HashSet::new();
        for (name, port) in ports {
            if *port == 0 {
                bail!("port '{name}' must be between 1 and 65535");
            }
            values.insert(*port);
            if self.ports.contains(port) {
                return Ok(false);
            }
        }

        let mut candidate_files = Vec::new();
        for port in values.iter().copied() {
            let file = open_lock(&self.directory.join(format!("{port}.lock")))?;
            if let Err(error) = file.try_lock_exclusive() {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(false);
                }
                return Err(error)
                    .with_context(|| format!("failed to acquire port lease for 127.0.0.1:{port}"));
            }
            if !port_is_available(port)? {
                return Ok(false);
            }
            candidate_files.push((port, file));
        }

        // Metadata is advisory, but only committed bundles should identify
        // themselves as owned. Failed probes leave unlocked, empty files.
        for (_, file) in &mut candidate_files {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            writeln!(
                file,
                "pid={} workspace={}",
                std::process::id(),
                workspace_root.display()
            )?;
            file.sync_data()?;
        }

        self.ports.extend(values);
        self.named_ports.extend(ports.clone());
        self.files
            .extend(candidate_files.into_iter().map(|(_, file)| file));
        Ok(true)
    }

    pub(crate) fn finish(
        self,
        workspace_root: &Path,
        services: BTreeMap<String, Option<String>>,
    ) -> Result<PortLease> {
        let manifest = AllocationManifest {
            version: 1,
            supervisor_pid: std::process::id(),
            workspace_root: canonical_workspace(workspace_root)?,
            ports: self.named_ports,
            services,
        };
        let manifest_path = write_manifest(&self.directory, &manifest)?;
        Ok(PortLease {
            files: self.files,
            manifest_path,
        })
    }
}

pub(crate) fn workspace_allocated_ports(
    workspace_root: &Path,
) -> Result<HashMap<String, BTreeSet<u16>>> {
    let workspace_root = canonical_workspace(workspace_root)?;
    let mut ports = HashMap::<String, BTreeSet<u16>>::new();
    for (_, manifest) in workspace_manifests(&workspace_root)? {
        for (name, port) in manifest.ports {
            ports.entry(name).or_default().insert(port);
        }
    }
    Ok(ports)
}

/// Return live allocations for one worktree. Crash-left manifests whose ports
/// are still occupied are retained as orphaned; fully stale manifests are
/// removed and omitted.
pub(crate) fn workspace_port_allocations(
    workspace_root: &Path,
) -> Result<Vec<WorkspacePortAllocation>> {
    let workspace_root = canonical_workspace(workspace_root)?;
    let directory = lease_directory();
    if !directory.exists() {
        return Ok(Vec::new());
    }

    let mut allocations = Vec::new();
    for (path, manifest) in workspace_manifests(&workspace_root)? {
        let status = match manifest_state(&directory, &manifest)? {
            ManifestState::Active => PortAllocationStatus::Active,
            ManifestState::Orphaned => PortAllocationStatus::Orphaned,
            ManifestState::Stale => {
                remove_stale_manifest(&path)?;
                continue;
            }
        };
        allocations.push(WorkspacePortAllocation {
            supervisor_pid: manifest.supervisor_pid,
            status,
            ports: manifest.ports,
            services: manifest.services,
        });
    }
    allocations.sort_by_key(|allocation| allocation.supervisor_pid);
    Ok(allocations)
}

/// Remove crash-left manifests only after every recorded lease is unlocked and
/// every recorded port is free. Active supervisors retain their manifests.
pub(crate) fn prune_workspace_manifests(workspace_root: &Path) -> Result<()> {
    let workspace_root = canonical_workspace(workspace_root)?;
    let directory = lease_directory();
    if !directory.exists() {
        return Ok(());
    }
    let allocation_lock = open_lock(&directory.join("allocation.lock"))?;
    allocation_lock
        .lock_exclusive()
        .context("failed to acquire the Aster port allocator lock")?;
    for (path, manifest) in workspace_manifests(&workspace_root)? {
        if manifest_state(&directory, &manifest)? == ManifestState::Stale {
            remove_stale_manifest(&path)?;
        }
    }
    Ok(())
}

/// What one allocation manifest says about its supervisor's ports.
#[derive(Debug, Eq, PartialEq)]
enum ManifestState {
    /// The manifest's own supervisor still holds a port lease.
    Active,
    /// No lease is held, but a recorded port is still listening: a crashed
    /// supervisor's service outlived it.
    Orphaned,
    /// Nothing the manifest names is held or listening for it.
    Stale,
}

/// Classify a manifest by who holds its port leases.
///
/// A held lease shows the manifest is live only when the lock's owner record
/// names the manifest's supervisor. After a supervisor dies its locks are
/// released, and a later supervisor may lease the same ports. That lease, and
/// the listener behind it, belong to the later supervisor and say nothing
/// about the dead one. A held lease with no readable owner record (an older
/// Aster wrote it, or the record is mid-write) counts as the manifest's own.
fn manifest_state(directory: &Path, manifest: &AllocationManifest) -> Result<ManifestState> {
    let mut unleased = Vec::new();
    for port in manifest.ports.values().copied().collect::<BTreeSet<_>>() {
        let path = directory.join(format!("{port}.lock"));
        let file = open_lock(&path)?;
        match file.try_lock_exclusive() {
            // Holding the lock here proves no supervisor holds it; dropping the
            // file at the end of this iteration releases it again.
            Ok(()) => unleased.push(port),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                match lease_owner_pid(&path) {
                    Some(owner) if owner != manifest.supervisor_pid => {}
                    _ => return Ok(ManifestState::Active),
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect port lease for {port}"));
            }
        }
    }
    for port in unleased {
        if !port_is_available(port)? {
            return Ok(ManifestState::Orphaned);
        }
    }
    Ok(ManifestState::Stale)
}

/// The supervisor PID recorded in a port lock by [`PortAllocator::try_bundle`].
fn lease_owner_pid(path: &Path) -> Option<u32> {
    let record = fs::read_to_string(path).ok()?;
    record
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))?
        .parse()
        .ok()
}

/// Remove a crash-left manifest. A concurrent reader may already have removed
/// it; that is success. Other errors keep the existing context message.
fn remove_stale_manifest(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove stale allocation manifest {}",
                path.display()
            )
        }),
    }
}

fn write_manifest(directory: &Path, manifest: &AllocationManifest) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    for suffix in 0..100_u8 {
        let stem = format!(
            "allocation-{}-{timestamp}-{suffix}",
            manifest.supervisor_pid
        );
        let temporary = directory.join(format!(".{stem}.tmp"));
        let path = directory.join(format!("{stem}.json"));
        let mut file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("failed to create allocation manifest"),
        };
        serde_json::to_writer_pretty(&mut file, manifest)
            .context("failed to encode allocation manifest")?;
        writeln!(file)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("failed to publish allocation manifest {}", path.display()))?;
        return Ok(path);
    }
    bail!("failed to choose a unique allocation manifest name")
}

fn workspace_manifests(workspace_root: &str) -> Result<Vec<(PathBuf, AllocationManifest)>> {
    let directory = lease_directory();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read port lease directory {}",
                    directory.display()
                )
            });
        }
    };
    let mut manifests = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("allocation-")
            || path.extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to open allocation manifest {}", path.display())
                });
            }
        };
        let manifest: AllocationManifest = serde_json::from_reader(file)
            .with_context(|| format!("failed to parse allocation manifest {}", path.display()))?;
        if manifest.version == 1 && manifest.workspace_root == workspace_root {
            manifests.push((path, manifest));
        }
    }
    Ok(manifests)
}

fn canonical_workspace(workspace_root: &Path) -> Result<String> {
    Ok(workspace_root
        .canonicalize()
        .with_context(|| {
            format!(
                "failed to canonicalize workspace root {}",
                workspace_root.display()
            )
        })?
        .to_string_lossy()
        .into_owned())
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open port lock {}", path.display()))
}

fn port_is_available(port: u16) -> Result<bool> {
    // On macOS, a listener created with SO_REUSEADDR on 0.0.0.0 can coexist
    // with a short-lived 127.0.0.1 bind probe. Connecting first catches that
    // listener, while the wildcard bind catches sockets that are bound but
    // not yet listening.
    if TcpStream::connect(("127.0.0.1", port)).is_ok() {
        return Ok(false);
    }

    match TcpListener::bind(("0.0.0.0", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to probe 0.0.0.0:{port}")),
    }
}

fn lease_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("ASTER_PORT_LEASE_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(unix)]
    let identity = unsafe { libc::geteuid() }.to_string();
    #[cfg(not(unix))]
    let identity = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir().join(format!("aster-port-leases-v1-{identity}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    fn lease_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct LeaseEnv {
        previous: Option<std::ffi::OsString>,
    }

    impl LeaseEnv {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("ASTER_PORT_LEASE_DIR");
            std::env::set_var("ASTER_PORT_LEASE_DIR", path);
            Self { previous }
        }
    }

    impl Drop for LeaseEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("ASTER_PORT_LEASE_DIR", value),
                None => std::env::remove_var("ASTER_PORT_LEASE_DIR"),
            }
        }
    }

    fn free_port() -> u16 {
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    fn write_stale_manifest(lease_dir: &Path, workspace: &Path, port: u16) -> PathBuf {
        let workspace_root = workspace
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let path = lease_dir.join("allocation-1-1-0.json");
        let body = serde_json::json!({
            "version": 1,
            "supervisor_pid": 1,
            "workspace_root": workspace_root,
            "ports": { "http": port }
        });
        fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        path
    }

    #[test]
    fn availability_probe_rejects_and_then_releases_external_listener() {
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_available(port).unwrap());

        drop(listener);
        assert!(port_is_available(port).unwrap());
    }

    fn manifest_for(pid: u32, port: u16) -> AllocationManifest {
        AllocationManifest {
            version: 1,
            supervisor_pid: pid,
            workspace_root: "/workspace".to_string(),
            ports: BTreeMap::from([("http".to_string(), port)]),
            services: BTreeMap::new(),
        }
    }

    /// Holds a port lease the way a supervisor does. flock locks belong to the
    /// open file description, so this handle blocks the classifier's own open.
    fn hold_lease(directory: &Path, port: u16, record: &str) -> File {
        let path = directory.join(format!("{port}.lock"));
        fs::write(&path, record).unwrap();
        let file = open_lock(&path).unwrap();
        file.try_lock_exclusive().unwrap();
        file
    }

    #[test]
    fn lease_held_by_the_manifest_supervisor_is_active() {
        let directory = tempfile::tempdir().unwrap();
        let port = free_port();
        let _lease = hold_lease(directory.path(), port, "pid=41 workspace=/workspace\n");
        assert_eq!(
            manifest_state(directory.path(), &manifest_for(41, port)).unwrap(),
            ManifestState::Active
        );
    }

    #[test]
    fn lease_taken_over_by_another_supervisor_leaves_the_manifest_stale() {
        let directory = tempfile::tempdir().unwrap();
        let port = free_port();
        let _lease = hold_lease(directory.path(), port, "pid=42 workspace=/workspace\n");
        assert_eq!(
            manifest_state(directory.path(), &manifest_for(41, port)).unwrap(),
            ManifestState::Stale
        );
    }

    #[test]
    fn held_lease_without_an_owner_record_counts_as_the_manifests_own() {
        let directory = tempfile::tempdir().unwrap();
        let port = free_port();
        let _lease = hold_lease(directory.path(), port, "");
        assert_eq!(
            manifest_state(directory.path(), &manifest_for(41, port)).unwrap(),
            ManifestState::Active
        );
    }

    #[test]
    fn unleased_port_that_still_listens_is_orphaned() {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            manifest_state(directory.path(), &manifest_for(41, port)).unwrap(),
            ManifestState::Orphaned
        );
        drop(listener);
        assert_eq!(
            manifest_state(directory.path(), &manifest_for(41, port)).unwrap(),
            ManifestState::Stale
        );
    }

    #[test]
    fn remove_stale_manifest_ignores_not_found_and_reports_other_errors() {
        let missing = std::env::temp_dir().join(format!(
            "aster-missing-allocation-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!missing.exists());
        remove_stale_manifest(&missing).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let temp = tempfile::tempdir().unwrap();
            let directory = temp.path().join("leases");
            fs::create_dir(&directory).unwrap();
            let path = directory.join("allocation-1-1-0.json");
            fs::write(&path, b"{}").unwrap();
            let _restore = DirectoryModeGuard::lock(&directory);
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();

            let error = remove_stale_manifest(&path).unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains(&format!(
                    "failed to remove stale allocation manifest {}",
                    path.display()
                )),
                "{message}"
            );
            assert!(
                message.to_ascii_lowercase().contains("permission denied"),
                "{message}"
            );
            assert!(path.exists());
        }
    }

    #[cfg(unix)]
    struct DirectoryModeGuard<'a> {
        path: &'a Path,
    }

    #[cfg(unix)]
    impl<'a> DirectoryModeGuard<'a> {
        fn lock(path: &'a Path) -> Self {
            Self { path }
        }
    }

    #[cfg(unix)]
    impl Drop for DirectoryModeGuard<'_> {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(self.path, fs::Permissions::from_mode(0o700));
        }
    }

    #[cfg(unix)]
    #[test]
    fn stale_manifest_removal_still_fails_when_the_lease_directory_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = lease_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let lease_dir = temp.path().join("leases");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&lease_dir).unwrap();
        let _lease_env = LeaseEnv::set(&lease_dir);
        let _restore = DirectoryModeGuard::lock(&lease_dir);

        let mut proved = false;
        for _ in 0..8 {
            fs::set_permissions(&lease_dir, fs::Permissions::from_mode(0o700)).unwrap();
            let port = free_port();
            if !port_is_available(port).unwrap() {
                continue;
            }
            let path = write_stale_manifest(&lease_dir, &workspace, port);
            // Lock files must already exist: a non-writable directory should fail
            // on unlink, not on creating the probe files.
            fs::write(lease_dir.join(format!("{port}.lock")), b"").unwrap();
            fs::write(lease_dir.join("allocation.lock"), b"").unwrap();
            fs::set_permissions(&lease_dir, fs::Permissions::from_mode(0o555)).unwrap();

            let allocations = workspace_port_allocations(&workspace);
            let pruned = prune_workspace_manifests(&workspace);
            fs::set_permissions(&lease_dir, fs::Permissions::from_mode(0o700)).unwrap();

            match (allocations, pruned) {
                (Err(allocations_error), Err(prune_error)) => {
                    for (label, error) in [
                        ("workspace_port_allocations", allocations_error),
                        ("prune_workspace_manifests", prune_error),
                    ] {
                        let message = format!("{error:#}");
                        assert!(
                            message.contains(&format!(
                                "failed to remove stale allocation manifest {}",
                                path.display()
                            )),
                            "{label}: {message}"
                        );
                        assert!(
                            message.to_ascii_lowercase().contains("permission denied"),
                            "{label}: {message}"
                        );
                    }
                    assert!(path.exists());
                    proved = true;
                    break;
                }
                (Ok(_), Ok(_)) => {
                    let _ = fs::remove_file(&path);
                }
                (allocations, pruned) => panic!(
                    "expected permission denied from both removers, or a skipped occupied port: {allocations:?} / {pruned:?}"
                ),
            }
        }
        assert!(
            proved,
            "port stayed occupied, so removal was never attempted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_manifests_skips_a_manifest_whose_read_returns_not_found() {
        let _env_lock = lease_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let lease_dir = temp.path().join("leases");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&lease_dir).unwrap();
        let _lease_env = LeaseEnv::set(&lease_dir);
        let workspace_root = canonical_workspace(&workspace).unwrap();
        // A directory entry that vanishes before open is NotFound. A dangling
        // symlink is that error without a scheduler race.
        std::os::unix::fs::symlink(
            lease_dir.join("missing-allocation.json"),
            lease_dir.join("allocation-2-1-0.json"),
        )
        .unwrap();

        assert!(workspace_manifests(&workspace_root).unwrap().is_empty());
    }

    #[test]
    fn concurrent_reader_and_prune_tolerate_a_removed_stale_manifest() {
        let _env_lock = lease_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let lease_dir = temp.path().join("leases");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&lease_dir).unwrap();
        let _lease_env = LeaseEnv::set(&lease_dir);

        let mut removed_a_stale_manifest = false;
        for _ in 0..8 {
            let port = free_port();
            if !port_is_available(port).unwrap() {
                continue;
            }
            let path = write_stale_manifest(&lease_dir, &workspace, port);
            let barrier = Arc::new(Barrier::new(2));
            let reader_workspace = workspace.clone();
            let pruner_workspace = workspace.clone();
            let reader_barrier = Arc::clone(&barrier);
            let pruner_barrier = Arc::clone(&barrier);
            let reader = std::thread::spawn(move || {
                reader_barrier.wait();
                workspace_port_allocations(&reader_workspace)
            });
            let pruner = std::thread::spawn(move || {
                pruner_barrier.wait();
                prune_workspace_manifests(&pruner_workspace)
            });
            // Same-process flock on {port}.lock can make one caller see the
            // other's probe as an active lease. That is not a supervisor, and
            // separate processes do not share that conflict. Both calls must
            // still succeed, and a free port's manifest must be gone.
            reader.join().unwrap().unwrap();
            pruner.join().unwrap().unwrap();
            if !path.exists() {
                removed_a_stale_manifest = true;
                continue;
            }
            if !port_is_available(port).unwrap() {
                let _ = fs::remove_file(&path);
                continue;
            }
            panic!("stale manifest was not removed from {}", path.display());
        }
        assert!(
            removed_a_stale_manifest,
            "never raced removal of a manifest whose port was free"
        );
    }
}
