use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

const TERMINATE_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default)]
pub struct KillPortsOptions {
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PortOwner {
    pid: u32,
    command: String,
    ports: BTreeSet<u16>,
}

pub fn resolve_port_selection(
    configured: &HashMap<String, u16>,
    requested: &[String],
) -> Result<Vec<u16>> {
    let mut selected = BTreeSet::new();
    if requested.is_empty() {
        selected.extend(configured.values().copied());
    } else {
        for value in requested {
            if let Some(port) = configured.get(value) {
                selected.insert(*port);
                continue;
            }
            let port = value.parse::<u16>().map_err(|_| {
                anyhow!("unknown configured port name or invalid port number '{value}'")
            })?;
            if port == 0 {
                bail!("port number must be between 1 and 65535");
            }
            selected.insert(port);
        }
    }
    if selected.is_empty() {
        bail!("no ports selected; configure [dev.ports] or provide port numbers");
    }
    Ok(selected.into_iter().collect())
}

pub fn kill_ports(ports: &[u16], options: KillPortsOptions) -> Result<()> {
    let owners = find_owners(ports)?;
    if owners.is_empty() {
        println!("No listeners found on {}.", format_ports(ports));
        return Ok(());
    }

    for owner in owners.values() {
        println!(
            "{}: process {} ({}) listening on {}",
            if options.dry_run {
                "Would terminate"
            } else {
                "Terminating"
            },
            owner.pid,
            owner.command,
            format_ports(&owner.ports.iter().copied().collect::<Vec<_>>())
        );
    }
    if options.dry_run {
        return Ok(());
    }

    terminate_owners(owners.values(), false)?;
    let deadline = Instant::now() + TERMINATE_GRACE;
    let mut remaining = find_owners(ports)?;
    while !remaining.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        remaining = find_owners(ports)?;
    }
    if !remaining.is_empty() {
        eprintln!("Force-killing {} remaining listener(s)...", remaining.len());
        terminate_owners(remaining.values(), true)?;
    }

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let remaining = find_owners(ports)?;
        if remaining.is_empty() {
            println!("Cleared {}.", format_ports(ports));
            return Ok(());
        }
        if Instant::now() >= deadline {
            let pids = remaining
                .values()
                .map(|owner| owner.pid.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "ports still occupied after cleanup: {} (listener PIDs: {pids})",
                format_ports(ports)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn format_ports(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|port| format!(":{port}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn find_owners(ports: &[u16]) -> Result<BTreeMap<u32, PortOwner>> {
    let mut owners = BTreeMap::<u32, PortOwner>::new();
    for &port in ports {
        for pid in listener_pids(port)? {
            let owner = owners.entry(pid).or_insert_with(|| PortOwner {
                pid,
                command: process_command(pid),
                ports: BTreeSet::new(),
            });
            owner.ports.insert(port);
        }
    }
    Ok(owners)
}

#[cfg(unix)]
fn listener_pids(port: u16) -> Result<Vec<u32>> {
    let output = match Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return listener_pids_without_lsof(port);
        }
        Err(error) => return Err(error).context("failed to inspect listening processes with lsof"),
    };
    if !output.status.success() && output.stdout.is_empty() {
        // lsof uses status 1 for an empty result.
        return Ok(Vec::new());
    }
    parse_pid_lines(&output.stdout, "lsof")
}

#[cfg(all(unix, target_os = "linux"))]
fn listener_pids_without_lsof(port: u16) -> Result<Vec<u32>> {
    linux_listener_pids(port)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn listener_pids_without_lsof(_port: u16) -> Result<Vec<u32>> {
    bail!("lsof is required to discover port owners on this operating system")
}

#[cfg(target_os = "linux")]
fn linux_listener_pids(port: u16) -> Result<Vec<u32>> {
    use std::fs;

    let mut inodes = BTreeSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let content = fs::read_to_string(table)
            .with_context(|| format!("failed to read Linux socket table {table}"))?;
        for line in content.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            let Some(hex_port) = fields[1].rsplit(':').next() else {
                continue;
            };
            if u16::from_str_radix(hex_port, 16).ok() == Some(port) {
                inodes.insert(fields[9].to_string());
            }
        }
    }
    if inodes.is_empty() {
        return Ok(Vec::new());
    }

    let mut pids = BTreeSet::new();
    for entry in fs::read_dir("/proc").context("failed to enumerate Linux processes")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            if let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|value| value.strip_suffix(']'))
            {
                if inodes.contains(inode) {
                    pids.insert(pid);
                    break;
                }
            }
        }
    }
    Ok(pids.into_iter().collect())
}

#[cfg(windows)]
fn listener_pids(port: u16) -> Result<Vec<u32>> {
    let output = Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
        .context("failed to inspect listening processes with netstat")?;
    if !output.status.success() {
        bail!("netstat failed while inspecting port {port}");
    }
    let mut pids = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 5 || !fields[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        if fields[1]
            .rsplit(':')
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            == Some(port)
        {
            if let Ok(pid) = fields[4].parse::<u32>() {
                pids.insert(pid);
            }
        }
    }
    Ok(pids.into_iter().collect())
}

#[cfg(unix)]
fn parse_pid_lines(stdout: &[u8], source: &str) -> Result<Vec<u32>> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim()
                .parse::<u32>()
                .with_context(|| format!("{source} returned invalid process ID {line:?}"))
        })
        .collect()
}

fn process_command(pid: u32) -> String {
    #[cfg(unix)]
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output();
    #[cfg(windows)]
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown command".to_string())
}

#[cfg(unix)]
fn terminate_owners<'a>(owners: impl Iterator<Item = &'a PortOwner>, force: bool) -> Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    for owner in owners {
        let target = i32::try_from(owner.pid).context("listener PID exceeds platform range")?;
        let result = unsafe { libc::kill(target, signal) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error)
                    .with_context(|| format!("failed to signal process target {target}"));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_owners<'a>(owners: impl Iterator<Item = &'a PortOwner>, force: bool) -> Result<()> {
    for owner in owners {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &owner.pid.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        let output = command.output().context("failed to run taskkill")?;
        if !output.status.success() {
            bail!(
                "taskkill failed for PID {}: {}",
                owner.pid,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_defaults_to_all_configured_ports_and_deduplicates() {
        let configured = HashMap::from([
            ("api".to_string(), 4011),
            ("alias".to_string(), 4011),
            ("web".to_string(), 3311),
        ]);
        assert_eq!(
            resolve_port_selection(&configured, &[]).unwrap(),
            vec![3311, 4011]
        );
    }

    #[test]
    fn selection_accepts_names_and_explicit_numbers() {
        let configured = HashMap::from([("api".to_string(), 4011)]);
        assert_eq!(
            resolve_port_selection(
                &configured,
                &["api".to_string(), "3311".to_string(), "4011".to_string()]
            )
            .unwrap(),
            vec![3311, 4011]
        );
    }

    #[test]
    fn selection_rejects_unknown_names_and_zero() {
        let configured = HashMap::new();
        assert!(resolve_port_selection(&configured, &["missing".to_string()]).is_err());
        assert!(resolve_port_selection(&configured, &["0".to_string()]).is_err());
        assert!(resolve_port_selection(&configured, &[]).is_err());
    }
}
