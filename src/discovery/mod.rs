//! Project discovery module
//!
//! Walks the workspace directory tree, finding projects by their marker files
//! and merging aster.toml overrides.

pub mod scanner;

pub use scanner::{discover_projects, DiscoveredProject};
