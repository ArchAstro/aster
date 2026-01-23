use anyhow::Result;
use std::collections::HashMap;
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

/// A build target with its command and dependencies
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    /// The command to execute for this target
    pub command: String,
    /// Target addresses that must run before this one (e.g., "//libs/shared:build", "//self:deps")
    /// Use "//self:target" to reference targets in the same project
    pub depends_on: Vec<String>,
}

/// Context passed to plugins for target detection
///
/// Contains all the raw information a plugin needs to determine targets
/// and their dependencies. The plugin is responsible for any language-specific
/// interpretation (e.g., resolving relative paths to project addresses).
#[derive(Debug)]
pub struct TargetContext<'a> {
    /// Path to the native config file (e.g., package.json, mix.exs)
    pub config_path: &'a Path,
    /// Project directory (parent of config_path)
    pub project_dir: &'a Path,
    /// Workspace root directory
    pub workspace_root: &'a Path,
    /// Dependencies parsed from native config
    pub dependencies: &'a [LocalDependency],
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

    /// Detect available targets from native config
    ///
    /// Returns only targets that actually exist in the project config.
    /// For Node.js: scripts that exist in package.json
    /// For Elixir: mix tasks (test/compile always available)
    /// For Python: based on pyproject.toml config
    ///
    /// Plugins should include a "deps" target for dependency installation
    /// and set depends_on for targets that require deps first.
    ///
    /// The plugin receives full context including raw dependencies and paths.
    /// It is responsible for resolving relative paths to project addresses
    /// and determining what cross-project dependencies are needed.
    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>>;
}

pub mod elixir;
pub mod nodejs;
pub mod python;
pub mod registry;

pub use elixir::ElixirPlugin;
pub use nodejs::NodeJsPlugin;
pub use python::PythonPlugin;
pub use registry::PluginRegistry;
