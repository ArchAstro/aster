use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fs2::FileExt;

/// OS-backed leases held for the lifetime of one service supervisor.
///
/// Dropping the files releases every lock, including when the process is
/// terminated without running application cleanup.
pub struct PortLease {
    #[allow(dead_code)]
    files: Vec<File>,
}

pub(crate) struct PortAllocator {
    directory: PathBuf,
    _allocation_lock: File,
    files: Vec<File>,
    ports: HashSet<u16>,
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
        self.files
            .extend(candidate_files.into_iter().map(|(_, file)| file));
        Ok(true)
    }

    pub(crate) fn finish(self) -> PortLease {
        PortLease { files: self.files }
    }
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

    #[test]
    fn availability_probe_rejects_and_then_releases_external_listener() {
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_available(port).unwrap());

        drop(listener);
        assert!(port_is_available(port).unwrap());
    }
}
