use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::DevWorkspaceConfig;

use super::port_allocator::{
    workspace_port_allocations, PortAllocationStatus, WorkspacePortAllocation,
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
    let instances = workspace_port_allocations(workspace_root)?
        .into_iter()
        .map(|allocation| report_instance(allocation, config))
        .collect();
    Ok(WorkspacePortsReport {
        workspace,
        instances,
    })
}

fn report_instance(
    allocation: WorkspacePortAllocation,
    config: &DevWorkspaceConfig,
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
            ServicePort {
                name,
                port_name,
                port,
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
                match instance.status {
                    ServicePortStatus::Active => "active".to_string(),
                    ServicePortStatus::Orphaned => "orphaned".to_string(),
                },
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
                match instance.status {
                    ServicePortStatus::Active => "active".to_string(),
                    ServicePortStatus::Orphaned => "orphaned".to_string(),
                },
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
