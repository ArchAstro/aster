pub mod address;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod executor;
pub mod git;
pub mod graph;
pub mod plugins;
pub mod targets;

// Re-export core types for convenience
pub use cli::{Cli, Commands};
pub use discovery::{discover_projects, DiscoveredProject};
pub use graph::{build_graph, find_cycle, CycleError, ProjectGraph, ProjectNode};
pub use plugins::{
    ElixirPlugin, LanguagePlugin, LocalDependency, NodeJsPlugin, PluginRegistry, ProjectMetadata,
    PythonPlugin,
};
pub use targets::TargetResolver;
pub use executor::{ExecutionResult, Executor};
pub use git::AffectedDetector;
