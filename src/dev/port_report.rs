use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::DevWorkspaceConfig;

use super::daemon::list_workspace_bundles;
use super::port_allocator::{
    workspace_port_allocations, PortAllocationStatus, WorkspacePortAllocation,
};
use super::service_report::{
    format_service_runtime_label, load_published_service_states, ServiceRuntimeState,
    ServiceRuntimeStatus,
};

#[derive(Debug, Serialize)]
pub struct WorkspacePortsReport {
    pub workspace: String,
    pub instances: Vec<ServicePortInstance>,
}

#[derive(Debug, Serialize)]
pub struct ServicePortInstance {
    pub supervisor_pid: u32,
    pub status: ServicePortStatus,
    pub services: Vec<ServicePort>,
    pub ports: BTreeMap<String, u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePortStatus {
    Active,
    Orphaned,
}

#[derive(Debug, Serialize)]
pub struct ServicePort {
    pub name: String,
    pub port_name: Option<String>,
    pub port: Option<u16>,
    /// Present when the supervisor published a state file for this service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_state: Option<ServiceRuntimeState>,
}

pub fn workspace_ports_report(
    workspace_root: &Path,
    config: &DevWorkspaceConfig,
) -> Result<WorkspacePortsReport> {
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
    let published = published_states_by_supervisor(workspace_root);
    let instances = workspace_port_allocations(workspace_root)?
        .into_iter()
        .map(|allocation| {
            let states = published
                .get(&allocation.supervisor_pid)
                .cloned()
                .unwrap_or_default();
            report_instance(allocation, config, &states)
        })
        .collect();
    Ok(WorkspacePortsReport {
        workspace,
        instances,
    })
}

fn published_states_by_supervisor(
    workspace_root: &Path,
) -> HashMap<u32, BTreeMap<String, ServiceRuntimeState>> {
    // Ports stay useful after the daemon socket is unreachable. A missing
    // bundle list or state file keeps today's lease status.
    let Ok(bundles) = list_workspace_bundles(workspace_root) else {
        return HashMap::new();
    };
    let published = load_published_service_states(&bundles);
    bundles
        .into_iter()
        .filter_map(|bundle| {
            published
                .get(&bundle.bundle_id)
                .cloned()
                .map(|states| (bundle.supervisor_pid, states))
        })
        .collect()
}

fn report_instance(
    allocation: WorkspacePortAllocation,
    config: &DevWorkspaceConfig,
    service_states: &BTreeMap<String, ServiceRuntimeState>,
) -> ServicePortInstance {
    // Older manifests have no service metadata. Preserve compatibility by
    // reconstructing their best available mapping from the current config.
    let service_ports = if allocation.services.is_empty() {
        config
            .services
            .iter()
            .map(|(name, service)| (name.clone(), service.port.clone()))
            .collect::<BTreeMap<_, _>>()
    } else {
        allocation.services
    };
    let mut services = service_ports
        .into_iter()
        .map(|(name, port_name)| {
            let port = port_name
                .as_ref()
                .and_then(|port_name| allocation.ports.get(port_name))
                .copied();
            let service_state = service_states.get(&name).cloned();
            ServicePort {
                name,
                port_name,
                port,
                service_state,
            }
        })
        .collect::<Vec<_>>();
    services.sort_by(|left, right| left.name.cmp(&right.name));

    ServicePortInstance {
        supervisor_pid: allocation.supervisor_pid,
        status: match allocation.status {
            PortAllocationStatus::Active => ServicePortStatus::Active,
            PortAllocationStatus::Orphaned => ServicePortStatus::Orphaned,
        },
        services,
        ports: allocation.ports,
    }
}

pub fn format_workspace_ports(report: &WorkspacePortsReport) -> String {
    if report.instances.is_empty() {
        return "No running service port allocations found for this worktree.\n".to_string();
    }

    let mut rows = Vec::new();
    for instance in &report.instances {
        for (port_name, port) in &instance.ports {
            let services = instance
                .services
                .iter()
                .filter(|service| service.port_name.as_deref() == Some(port_name))
                .map(|service| service.name.as_str())
                .collect::<Vec<_>>()
                .join(",");
            rows.push([
                if services.is_empty() {
                    "-".to_string()
                } else {
                    services
                },
                port_name.clone(),
                port.to_string(),
                format_lease_status(
                    instance.status,
                    &instance
                        .services
                        .iter()
                        .filter(|service| service.port_name.as_deref() == Some(port_name))
                        .map(|service| service.service_state.as_ref())
                        .collect::<Vec<_>>(),
                ),
                instance.supervisor_pid.to_string(),
            ]);
        }
        for service in instance
            .services
            .iter()
            .filter(|service| service.port_name.is_none())
        {
            rows.push([
                service.name.clone(),
                "-".to_string(),
                "-".to_string(),
                format_lease_status(instance.status, &[service.service_state.as_ref()]),
                instance.supervisor_pid.to_string(),
            ]);
        }
    }

    let headers = ["SERVICE", "PORT NAME", "PORT", "STATUS", "SUPERVISOR"];
    let widths = std::array::from_fn::<_, 5, _>(|column| {
        rows.iter()
            .map(|row| row[column].len())
            .chain(std::iter::once(headers[column].len()))
            .max()
            .unwrap_or(0)
    });
    let mut output = String::new();
    writeln!(
        output,
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3]
    )
    .expect("writing to a String cannot fail");
    for row in rows {
        writeln!(
            output,
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3]
        )
        .expect("writing to a String cannot fail");
    }
    output
}

fn format_lease_status(
    status: ServicePortStatus,
    states: &[Option<&ServiceRuntimeState>],
) -> String {
    let lease = match status {
        ServicePortStatus::Active => "active",
        ServicePortStatus::Orphaned => "orphaned",
    };
    let exited = states
        .iter()
        .flatten()
        .filter(|state| state.state == ServiceRuntimeStatus::Exited)
        .copied()
        .collect::<Vec<_>>();
    if exited.is_empty() {
        return lease.to_string();
    }
    if exited.len() == 1 {
        return format!("{lease}, {}", format_service_runtime_label(exited[0]));
    }
    let detail = exited
        .iter()
        .map(|state| format_service_runtime_label(state))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{lease}, {detail}")
}
