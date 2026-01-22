pub mod address;
pub mod config;
pub mod discovery;
pub mod plugins;

// Re-export core types for convenience
pub use discovery::{discover_projects, DiscoveredProject};
pub use plugins::{LanguagePlugin, LocalDependency, NodeJsPlugin, PluginRegistry, ProjectMetadata};
