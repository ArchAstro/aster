use anyhow::Result;
use std::collections::{HashMap, HashSet};
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

/// Cache inputs provided by a language plugin
///
/// Defines the files and environment variables that should be included
/// in the cache key for targets of this language type.
#[derive(Debug, Clone, Default)]
pub struct CacheInputs {
    /// Glob patterns for source files (e.g., ["src/**/*.ts", "lib/**/*.ts"])
    pub source_globs: Vec<String>,
    /// Specific config/lock files to include (e.g., ["package.json", "package-lock.json"])
    pub config_files: Vec<String>,
    /// Environment variables to track (e.g., ["NODE_ENV", "CI"])
    pub env_vars: Vec<String>,
}

/// Capabilities that a target may support
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TargetCapability {
    /// Target can accept a list of files to operate on
    /// (e.g., run tests only for specific files)
    FilesList,
    /// Target can treat warnings as errors
    /// (e.g., fail build on compiler warnings)
    WarningsAsErrors,
}

/// A build target with its command and dependencies
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    /// The command to execute for this target
    /// May contain {files} placeholder for file injection
    pub command: String,
    /// Target addresses that must run before this one (e.g., "//libs/shared:build", "//self:deps")
    /// Use "//self:target" to reference targets in the same project
    pub depends_on: Vec<String>,
    /// Capabilities this target supports
    pub capabilities: HashSet<TargetCapability>,
    /// Optional glob pattern to filter files for FilesList capability
    /// e.g., "*_test.go" or "*.spec.ts"
    pub files_glob: Option<String>,
    /// Stream output to stdout in real-time (for long-running processes like dev servers)
    pub stream: bool,
    /// Cache configuration overrides from aster.toml
    pub cache: Option<crate::config::CacheConfig>,
    /// When true, invalidates all cache entries for this project after successful execution
    pub invalidates_cache: bool,
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

    /// Modify a command to operate on specific files
    ///
    /// Called when a target has the FilesList capability and files are provided.
    /// The plugin can filter/transform files as appropriate for its tooling.
    ///
    /// - target_name: name of the target (e.g., "test")
    /// - command: the original command
    /// - files: list of file paths (relative to project directory)
    ///
    /// Returns Some(modified_command) if files should be passed,
    /// or None to run the original command unchanged.
    fn with_files_list(
        &self,
        _target_name: &str,
        _command: &str,
        _files: &[PathBuf],
    ) -> Option<String> {
        // Default: don't modify command (run full target)
        None
    }

    /// Modify a command to treat warnings as errors
    ///
    /// Called when a target has the WarningsAsErrors capability and
    /// --warnings-as-errors is passed on the command line.
    ///
    /// - target_name: name of the target (e.g., "build", "lint", "test")
    /// - command: the original command
    ///
    /// Returns Some(modified_command) if the target supports warnings-as-errors,
    /// or None if not supported for this target.
    fn with_warnings_as_errors(&self, _target_name: &str, _command: &str) -> Option<String> {
        // Default: don't modify command (not supported)
        None
    }

    /// Provide cache inputs for a target
    ///
    /// Returns the default files and environment variables that should be
    /// included in the cache key for targets of this language type.
    /// Users can extend or override these via aster.toml.
    ///
    /// - target_name: name of the target (e.g., "test", "build")
    fn cache_inputs(&self, _target_name: &str) -> CacheInputs {
        // Default: empty inputs (no caching without plugin support)
        CacheInputs::default()
    }
}

pub mod elixir;
pub mod go;
pub mod nodejs;
pub mod python;
pub mod registry;
pub mod rust;

pub use elixir::ElixirPlugin;
pub use go::GoPlugin;
pub use nodejs::NodeJsPlugin;
pub use python::PythonPlugin;
pub use registry::PluginRegistry;
pub use rust::RustPlugin;

// Re-export CacheInputs for use by cache module
pub use self::CacheInputs as PluginCacheInputs;
