use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::DevWorkspaceConfig;

use super::daemon::{BundleDescriptor, BundleState};

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceBundleState {
    Starting,
    Running,
    Stopping,
}

pub fn service_bundles_report(
    workspace_root: &Path,
    bundles: Vec<BundleDescriptor>,
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
                state_name(bundle.state).to_string(),
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
                state_name(bundle.state).to_string(),
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
                    state_name(bundle.state).to_string(),
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

fn state_name(state: ServiceBundleState) -> &'static str {
    match state {
        ServiceBundleState::Starting => "starting",
        ServiceBundleState::Running => "running",
        ServiceBundleState::Stopping => "stopping",
    }
}
