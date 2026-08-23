//! Git integration for affected detection
//!
//! Provides utilities to detect changed files between git refs and map them
//! to affected projects in the workspace.

pub mod affected;
pub mod file_owner;
pub mod ignore;
pub mod lane;

pub use affected::AffectedDetector;
pub use file_owner::{affected_with_dependents, files_to_projects};
pub use ignore::AffectedIgnore;
pub use lane::select_affected_lane;
