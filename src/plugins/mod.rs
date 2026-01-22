use anyhow::Result;
use std::path::{Path, PathBuf};

/// Metadata extracted from a project's native config file
#[derive(Debug, Clone)]
pub struct ProjectMetadata {
    /// Project name (from package.json "name", mix.exs project name, etc.)
    pub name: String,
    /// Optional version string
    pub version: Option<String>,
}

/// A local/path dependency declared in native config
#[derive(Debug, Clone)]
pub struct LocalDependency {
    /// Dependency name as declared in config
    pub name: String,
    /// Resolved path to the dependency (relative to workspace root)
    pub path: PathBuf,
}

/// Trait that each language plugin implements
pub trait LanguagePlugin: Send + Sync {
    /// Plugin identifier (e.g., "nodejs", "elixir")
    fn name(&self) -> &str;

    /// Files that identify this project type (e.g., ["package.json"])
    fn marker_files(&self) -> &[&str];

    /// Parse native config to extract project metadata
    /// - root: workspace root directory
    /// - config_path: path to the config file (e.g., /workspace/services/api/package.json)
    fn parse_project(&self, root: &Path, config_path: &Path) -> Result<ProjectMetadata>;

    /// Extract local dependencies from native config
    ///
    /// - config_path: path to the config file
    ///
    /// Returns paths relative to the project directory
    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>>;
}

pub mod elixir;
pub mod nodejs;
pub mod python;
pub mod registry;

pub use elixir::ElixirPlugin;
pub use nodejs::NodeJsPlugin;
pub use python::PythonPlugin;
pub use registry::PluginRegistry;
