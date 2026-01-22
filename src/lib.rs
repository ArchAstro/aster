pub mod address;
pub mod config;
pub mod plugins;

// Re-export core types for convenience
pub use plugins::{LanguagePlugin, LocalDependency, PluginRegistry, ProjectMetadata};
