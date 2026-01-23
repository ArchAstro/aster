//! Dependency graph construction and analysis
//!
//! Builds directed graphs from discovered projects and provides
//! cycle detection and topological ordering.
//!
//! Two graph types are available:
//! - `ProjectGraph`: project-level dependencies (//project -> //project)
//! - `TargetGraph`: target-level dependencies (//project:target -> //project:target)

pub mod builder;
pub mod cycles;
pub mod path;
pub mod targets;

pub use builder::{build_graph, ProjectGraph, ProjectNode};
pub use cycles::{find_cycle, CycleError};
pub use path::{find_path, format_path};
pub use targets::{build_target_graph, TargetGraph, TargetNode};
