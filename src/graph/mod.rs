//! Dependency graph construction and analysis
//!
//! Builds a directed graph from discovered projects and provides
//! cycle detection and topological ordering.

pub mod builder;
pub mod cycles;
pub mod path;

pub use builder::{build_graph, ProjectGraph, ProjectNode};
pub use cycles::{find_cycle, CycleError};
pub use path::{find_path, format_path};
