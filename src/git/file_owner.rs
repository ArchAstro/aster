//! File-to-project ownership mapping
//!
//! Maps changed files to the projects that own them and expands to include
//! transitive dependents.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::discovery::DiscoveredProject;
use crate::graph::ProjectGraph;

/// Map changed files to the projects that own them
///
/// A project owns a file if the file path starts with the project's relative_path.
/// Projects are sorted by path length (longest first) to match most specific project.
///
/// Files that don't belong to any project (e.g., workspace root files like README.md)
/// are ignored.
pub fn files_to_projects(
    _changed_files: &HashSet<PathBuf>,
    _projects: &[DiscoveredProject],
) -> HashSet<String> {
    // TODO: Implement in Task 2
    HashSet::new()
}

/// Expand directly affected projects to include all their dependents
///
/// Uses BFS to find all projects that transitively depend on the affected projects.
pub fn affected_with_dependents(
    _directly_affected: HashSet<String>,
    _graph: &ProjectGraph,
) -> HashSet<String> {
    // TODO: Implement in Task 2
    HashSet::new()
}
