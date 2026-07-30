pub mod address;
pub mod cache;
pub mod cli;
pub mod config;
pub mod dev;
pub mod discovery;
pub mod executor;
pub mod git;
pub mod graph;
pub mod plugins;
pub mod targets;
pub mod ui;
pub mod watch;
#[cfg(windows)]
mod windows_process;

// Re-export core types for convenience
pub use cli::{Cli, Commands};
pub use discovery::{discover_projects, DiscoveredProject};
pub use executor::{ExecutionResult, Executor};
pub use git::AffectedDetector;
pub use graph::{build_graph, find_cycle, CycleError, ProjectGraph, ProjectNode};
pub use plugins::{
    ElixirPlugin, GoPlugin, LanguagePlugin, LocalDependency, NodeJsPlugin, PluginRegistry,
    ProjectMetadata, PythonPlugin,
};
pub use targets::TargetResolver;
pub use ui::ProgressDisplay;
