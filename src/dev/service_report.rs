use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::DevWorkspaceConfig;

use super::daemon::{BundleDescriptor, BundleState};

/// Must match `READY_ID_ENV` in `daemon.rs`. The supervisor is the only
/// process that receives it, so a foreground `services up` does not publish.
const DAEMON_BUNDLE_ID_ENV: &str = "ASTER_INTERNAL_DAEMON_BUNDLE_ID";

/// Stable, scriptable report of daemon-owned bundles in one canonical worktree.
#[derive(Debug, Serialize)]
pub struct ServiceBundlesReport {
    pub workspace: String,
    pub bundles: Vec<ServiceBundleReport>,
}

#[derive(Debug, Serialize)]
pub struct ServiceBundleReport {
    /// Normalized daemon identity (`__aster_default__` for the default bundle).
    pub group: String,
    /// User-facing group name (`default` for the default bundle).
    pub display_group: String,
    pub state: ServiceBundleState,
    pub supervisor_pid: u32,
    pub services: Vec<String>,
    pub ports: BTreeMap<String, u16>,
    /// Per-service runtime state from the supervisor state file. Omitted when
    /// that file is missing or unreadable so existing JSON stays unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub service_state: BTreeMap<String, ServiceRuntimeState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceBundleState {
    Starting,
    Running,
    Stopping,
}

/// One service's published runtime state. Additive on list/ports JSON.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceRuntimeState {
    pub state: ServiceRuntimeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ServiceRuntimeExit>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRuntimeStatus {
    Running,
    Exited,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ServiceRuntimeExit {
    Code {
        code: i32,
    },
    Signal {
        signal: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// Path of `service-state-<bundle_id>.json` in the daemon runtime directory.
///
/// The directory rule matches `daemon.rs` `runtime_directory`: an explicit
/// `ASTER_DAEMON_RUNTIME_DIR`, otherwise the per-uid temp directory.
pub fn published_service_state_path(bundle_id: &str) -> Option<PathBuf> {
    if !bundle_id_is_file_safe(bundle_id) {
        return None;
    }
    Some(daemon_runtime_directory().join(format!("service-state-{bundle_id}.json")))
}

/// Read each bundle's state file. A missing or unreadable file is omitted so
/// callers keep today's bundle-level state.
pub fn load_published_service_states(
    bundles: &[BundleDescriptor],
) -> BTreeMap<String, BTreeMap<String, ServiceRuntimeState>> {
    let mut loaded = BTreeMap::new();
    for bundle in bundles {
        if let Some(states) = read_published_service_state(&bundle.bundle_id) {
            loaded.insert(bundle.bundle_id.clone(), states);
        }
    }
    loaded
}

pub fn format_service_runtime_label(state: &ServiceRuntimeState) -> String {
    match state.state {
        ServiceRuntimeStatus::Running => "running".to_string(),
        ServiceRuntimeStatus::Exited => match &state.exit {
            Some(ServiceRuntimeExit::Code { code }) => format!("exited (code {code})"),
            Some(ServiceRuntimeExit::Signal {
                signal: _,
                name: Some(name),
            }) if signal_name_is_display_safe(name) => {
                format!("exited (signal {name})")
            }
            Some(ServiceRuntimeExit::Signal { signal, .. }) => {
                format!("exited (signal {signal})")
            }
            None => "exited".to_string(),
        },
    }
}

fn read_published_service_state(bundle_id: &str) -> Option<BTreeMap<String, ServiceRuntimeState>> {
    let path = published_service_state_path(bundle_id)?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn bundle_id_is_file_safe(bundle_id: &str) -> bool {
    !bundle_id.is_empty()
        && bundle_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn signal_name_is_display_safe(name: &str) -> bool {
    name.starts_with("SIG")
        && name.len() > 3
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn daemon_runtime_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("ASTER_DAEMON_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        std::env::temp_dir().join(format!("aster-daemon-v1-{uid}"))
    }
    #[cfg(not(unix))]
    std::env::temp_dir().join("aster-daemon-v1")
}

pub fn daemon_bundle_id_from_env() -> Option<String> {
    std::env::var(DAEMON_BUNDLE_ID_ENV)
        .ok()
        .filter(|bundle_id| bundle_id_is_file_safe(bundle_id))
}

pub fn service_bundles_report(
    workspace_root: &Path,
    bundles: Vec<BundleDescriptor>,
    published_states: &BTreeMap<String, BTreeMap<String, ServiceRuntimeState>>,
) -> Result<ServiceBundlesReport> {
    let workspace = workspace_root
        .canonicalize()
        .with_context(|| {
            format!(
                "failed to canonicalize workspace root {}",
                workspace_root.display()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let mut bundles = bundles
        .into_iter()
        .map(|bundle| {
            let mut services = bundle.services;
            services.sort();
            services.dedup();
            ServiceBundleReport {
                display_group: bundle
                    .display_group
                    .unwrap_or_else(|| "default".to_string()),
                group: bundle.group,
                state: match bundle.state {
                    BundleState::Starting => ServiceBundleState::Starting,
                    BundleState::Running => ServiceBundleState::Running,
                    BundleState::Stopping => ServiceBundleState::Stopping,
                },
                supervisor_pid: bundle.supervisor_pid,
                services,
                ports: bundle.ports,
                service_state: published_states
                    .get(&bundle.bundle_id)
                    .cloned()
                    .unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    bundles.sort_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then_with(|| left.services.cmp(&right.services))
            .then_with(|| left.supervisor_pid.cmp(&right.supervisor_pid))
    });
    Ok(ServiceBundlesReport { workspace, bundles })
}

pub fn format_service_bundles(
    report: &ServiceBundlesReport,
    config: &DevWorkspaceConfig,
) -> String {
    if report.bundles.is_empty() {
        return "No daemon-managed service bundles found for this worktree.\n".to_string();
    }

    let mut rows: Vec<[String; 6]> = Vec::new();
    for bundle in &report.bundles {
        if bundle.services.is_empty() && bundle.ports.is_empty() {
            rows.push([
                terminal_safe(&bundle.display_group),
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                state_label(bundle, None),
                bundle.supervisor_pid.to_string(),
            ]);
        }
        for service in &bundle.services {
            let configured_port = config
                .services
                .get(service)
                .and_then(|service| service.port.as_deref())
                .or_else(|| {
                    service.strip_suffix("-proxy").and_then(|upstream| {
                        config
                            .services
                            .get(upstream)
                            .and_then(|service| service.proxy.as_ref().and(service.port.as_deref()))
                    })
                });
            let (port_name, port) = configured_port
                .and_then(|name| bundle.ports.get(name).map(|port| (name, *port)))
                .map_or_else(
                    || ("-".to_string(), "-".to_string()),
                    |(name, port)| (name.to_string(), port.to_string()),
                );
            rows.push([
                terminal_safe(&bundle.display_group),
                terminal_safe(service),
                terminal_safe(&port_name),
                port,
                state_label(bundle, Some(service)),
                bundle.supervisor_pid.to_string(),
            ]);
        }
        // Preserve evidence for named ports not associated with a currently
        // configured service while still emitting exactly one row per service.
        for (name, port) in &bundle.ports {
            if !bundle.services.iter().any(|service| {
                config
                    .services
                    .get(service)
                    .and_then(|service| service.port.as_deref())
                    == Some(name.as_str())
            }) {
                rows.push([
                    terminal_safe(&bundle.display_group),
                    "-".to_string(),
                    terminal_safe(name),
                    port.to_string(),
                    state_label(bundle, None),
                    bundle.supervisor_pid.to_string(),
                ]);
            }
        }
    }
    rows.sort();

    let headers = [
        "GROUP",
        "SERVICE",
        "PORT NAME",
        "PORT",
        "STATE",
        "SUPERVISOR",
    ];
    let widths = std::array::from_fn::<_, 6, _>(|column| {
        rows.iter()
            .map(|row| row[column].len())
            .chain(std::iter::once(headers[column].len()))
            .max()
            .unwrap_or(0)
    });
    let mut output = String::new();
    writeln!(
        output,
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4]
    )
    .expect("writing to a String cannot fail");
    for row in rows {
        writeln!(
            output,
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4]
        )
        .expect("writing to a String cannot fail");
    }
    output
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn state_label(bundle: &ServiceBundleReport, service: Option<&str>) -> String {
    service
        .and_then(|service| bundle.service_state.get(service))
        .map(format_service_runtime_label)
        .unwrap_or_else(|| state_name(bundle.state).to_string())
}

fn state_name(state: ServiceBundleState) -> &'static str {
    match state {
        ServiceBundleState::Starting => "starting",
        ServiceBundleState::Running => "running",
        ServiceBundleState::Stopping => "stopping",
    }
}
