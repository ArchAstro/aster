//! Named project-address filtering for `aster affected` primary projects.

use anyhow::{anyhow, Result};
use std::collections::HashSet;

use crate::address::ProjectSelector;
use crate::config::{AffectedLaneConfig, AffectedWorkspaceConfig};
use crate::discovery::DiscoveredProject;

/// The configured lane named `lane_name`.
pub fn find_lane<'a>(
    lane_name: &str,
    config: &'a AffectedWorkspaceConfig,
) -> Result<&'a AffectedLaneConfig> {
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
    // Matrix fields share the row with the fields Aster writes.
    // A lane has no `lane` key — the table name is the lane — so the
    // advice is always to rename the matrix field, never to set it on the lane.
    for key in lane.matrix.keys() {
        if MATRIX_ROW_KEYS.contains(&key.as_str()) {
            let reserved = MATRIX_ROW_KEYS.join(", ");
            return Err(anyhow!(
                "affected lane '{lane_name}' matrix field '{key}' is reserved \
                 (reserved: {reserved}); rename it"
            ));
        }
    }
    Ok(lane)
}

/// Fields Aster writes into every `--matrix` row.
pub const MATRIX_ROW_KEYS: [&str; 4] = ["lane", "name", "target", "toolchains"];

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
    let lane = find_lane(lane_name, config)?;

    let addresses = projects
        .iter()
        .map(|project| format!("//{}", project.relative_path.display()))
        .collect::<Vec<_>>();
    let parse = |kind: &str, value: &str| -> Result<ProjectSelector> {
        let selector = ProjectSelector::parse(value).map_err(|error| {
            anyhow!("invalid selector '{value}' in affected lane '{lane_name}' {kind}: {error}")
        })?;
        if !addresses.iter().any(|address| selector.matches(address)) {
            return Err(anyhow!(
                "selector '{value}' in affected lane '{lane_name}' {kind} matches no projects"
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
