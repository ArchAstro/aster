//! Named project-address filtering for `aster affected` primary projects.

use anyhow::{anyhow, Result};
use std::collections::HashSet;

use crate::address::ProjectSelector;
use crate::config::AffectedWorkspaceConfig;
use crate::discovery::DiscoveredProject;

/// Apply a configured lane to affected project addresses.
///
/// Every configured selector must match at least one discovered project. Empty
/// includes mean all affected projects, and exclusions always win.
pub fn select_affected_lane(
    lane_name: &str,
    config: &AffectedWorkspaceConfig,
    affected: HashSet<String>,
    projects: &[DiscoveredProject],
) -> Result<HashSet<String>> {
    let lane = config.lanes.get(lane_name).ok_or_else(|| {
        let mut known = config.lanes.keys().cloned().collect::<Vec<_>>();
        known.sort();
        if known.is_empty() {
            anyhow!("unknown affected lane '{lane_name}'; no lanes are configured")
        } else {
            anyhow!(
                "unknown affected lane '{}'; configured lanes: {}",
                lane_name,
                known.join(", ")
            )
        }
    })?;

    let addresses = projects
        .iter()
        .map(|project| format!("//{}", project.relative_path.display()))
        .collect::<Vec<_>>();
    let parse = |kind: &str, value: &str| -> Result<ProjectSelector> {
        let selector = ProjectSelector::parse(value).map_err(|error| {
            anyhow!(
                "invalid selector '{}' in affected lane '{}' {}: {}",
                value,
                lane_name,
                kind,
                error
            )
        })?;
        if !addresses.iter().any(|address| selector.matches(address)) {
            return Err(anyhow!(
                "selector '{}' in affected lane '{}' {} matches no projects",
                value,
                lane_name,
                kind
            ));
        }
        Ok(selector)
    };

    let includes = lane
        .include
        .iter()
        .map(|value| parse("include", value))
        .collect::<Result<Vec<_>>>()?;
    let excludes = lane
        .exclude
        .iter()
        .map(|value| parse("exclude", value))
        .collect::<Result<Vec<_>>>()?;

    Ok(affected
        .into_iter()
        .filter(|address| {
            (includes.is_empty() || includes.iter().any(|selector| selector.matches(address)))
                && !excludes.iter().any(|selector| selector.matches(address))
        })
        .collect())
}
