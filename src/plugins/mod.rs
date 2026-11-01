use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::test_results::{ReportContext, ReportPlan};

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

/// A warm-start directory for a native tool, restored by actions/cache.
///
/// These caches are not content-addressed and are distinct from Aster's
/// artifact cache. `shared` marks a machine-global directory (for example
/// `~/.cargo/registry`) so a later layer can dedupe it across projects.
/// Paths and key files that do not start with `~` are relative to the Aster
/// workspace root, because actions/cache runs from the repo checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCache {
    /// Stable id. Per-workspace caches include the cargo root, for example
    /// `cargo-target:crates/app`.
    pub id: String,
    /// Directories to save and restore. `~` is the user home directory.
    pub paths: Vec<String>,
    /// Files hashed into the actions/cache key, relative to the workspace root.
    pub key_files: Vec<String>,
    /// Command whose output is hashed into the key, when the toolchain matters.
    pub key_runtime: Option<String>,
    /// Machine-global cache, deduped across projects.
    pub shared: bool,
}

/// Every file under the project.
///
/// Go, Node, Elixir, Rust, and Python use this for every target except
/// `deps`. The hasher still omits gitignored paths, nested projects, tool
/// caches, and declared outputs, so the glob does not hash a build's own
/// artifacts.
pub const PROJECT_FILES_GLOB: &str = "**/*";

/// Cache inputs provided by a language plugin
///
/// Defines the files and environment variables that should be included
/// in the cache key for targets of this language type.
#[derive(Debug, Clone, Default)]
pub struct CacheInputs {
    /// Glob patterns for source files. Language plugins other than `deps`
    /// use [`PROJECT_FILES_GLOB`] so a testdata file, embed, or tool config
    /// misses the cache.
    pub source_globs: Vec<String>,
    /// Specific config/lock files to include (e.g., ["package.json", "package-lock.json"])
    pub config_files: Vec<String>,
    /// Files outside the project, relative to the workspace root.
    ///
    /// A Rust member's key includes the workspace `Cargo.lock`; a nested
    /// Node project includes the root lockfile. Patterns may be literals
    /// or globs. They are hashed with a `ws:` prefix so `Cargo.lock` here
    /// cannot collide with the member's own `Cargo.lock`.
    pub workspace_files: Vec<String>,
    /// Environment variables to track (e.g., ["NODE_ENV", "MIX_ENV"])
    ///
    /// A name ending in `*` is a prefix: every snapshot variable that starts
    /// with the name without that `*` is hashed, sorted by name.
    ///
    /// `CI` is not tracked by default. A key computed in GitHub Actions must
    /// match one computed on a laptop for identical inputs. Users who want
    /// `CI` in the key opt in with `[targets.<name>.cache] env = ["CI"]`.
    pub env_vars: Vec<String>,
    /// Environment variables whose value is a filesystem path.
    ///
    /// Exact names only; prefix patterns stay on `env_vars`. A list value
    /// (`PYTHONPATH`) is split on the platform path separator. Each entry
    /// under the workspace root is hashed workspace-relative; an entry
    /// outside it (for example `JAVA_HOME`) is hashed as its final path
    /// component, so two machines with the same install name share a key.
    /// The toolchain version stays in `runtime_commands`.
    pub path_env_vars: Vec<String>,
    /// Commands whose trimmed stdout+stderr is hashed into the cache key.
    ///
    /// Plugin defaults are toolchain version commands (`rustc -vV`,
    /// `node --version`). A target's `[targets.<name>.cache] runtime` replaces
    /// this list; `runtime = []` disables runtime inputs.
    pub runtime_commands: Vec<String>,
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

/// How a files-list target should run for a set of changed files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesListPlan {
    /// Run the target's command unchanged.
    Full,
    /// Run these commands in order instead of the target's command. The
    /// target fails at the first command that fails.
    Commands(Vec<String>),
    /// None of the changed files is relevant to this target.
    Nothing,
    /// The plugin's `with_files_list` declined to rewrite the command. Plugins
    /// differ on whether that means "nothing to run" or "run everything", so
    /// a requested target is skipped (the long-standing behaviour) while a
    /// prerequisite target runs in full.
    Declined,
}

/// A [`FilesListPlan`] plus the reasoning behind it, shown by `--dry-run` and
/// `--verbose`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesListSelection {
    /// What to run.
    pub plan: FilesListPlan,
    /// Human-readable lines explaining how the plan was chosen.
    pub explanation: Vec<String>,
}

impl FilesListSelection {
    /// A selection with no explanation.
    pub fn new(plan: FilesListPlan) -> Self {
        Self {
            plan,
            explanation: Vec::new(),
        }
    }
}

/// A test file selected by `aster affected --related`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedTest {
    /// Path relative to the project directory.
    pub file: PathBuf,
    /// Test names to run within the file; empty for the whole file.
    pub names: Vec<String>,
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
    /// Override working directory for this target (absolute path)
    /// If None, uses the project's root directory
    pub working_dir: Option<PathBuf>,
    /// Named resources that require exclusive access within a DAG level.
    /// Targets sharing a resource are serialized via per-resource mutexes
    /// while non-contending targets still run in parallel.
    pub exclusive_resources: Vec<String>,
    /// Target addresses whose runs this target joins (`selected_by` in
    /// aster.toml). Not a dependency: it orders nothing and adds no edge.
    pub selected_by: Vec<String>,
    /// Shell command run in the target's working directory before each
    /// `--retry-failed` rerun, such as a database reset. A failing setup
    /// skips that retry.
    pub retry_setup: Option<String>,
    /// The test report the target's command writes, declared in aster.toml.
    pub report: Option<crate::config::ReportConfig>,
}

impl Target {
    /// Dependency-only targets have no command, process, or stored result.
    pub fn is_dependency_only(&self) -> bool {
        self.command.is_empty() && !self.depends_on.is_empty()
    }
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
    /// Internal plugin identifier (e.g., "nodejs", "gradle", "maven")
    fn name(&self) -> &str;

    /// Languages used by the discovered project.
    ///
    /// Most plugins represent one language, so their plugin identifier is the
    /// language name. Build-system plugins can override this to detect one or
    /// more source languages independently from the backend used to execute
    /// targets.
    fn languages(&self, _project_dir: &Path, _config_path: &Path) -> Result<Vec<String>> {
        Ok(vec![self.name().to_string()])
    }

    /// Build system used to execute this project's targets, when distinct
    /// from its language identity.
    fn build_system(&self) -> Option<&str> {
        None
    }

    /// Files that identify this project type (e.g., ["package.json"])
    fn marker_files(&self) -> &[&str];

    /// Whether this plugin recognizes a marker filename.
    ///
    /// Most plugins use exact marker filenames. Build systems such as Gradle
    /// may also use project-specific build filenames, so they can override
    /// this method and let `should_skip` apply the path-aware filtering.
    fn matches_marker(&self, filename: &str) -> bool {
        self.marker_files().contains(&filename)
    }

    /// Check if this config file should be skipped during discovery
    ///
    /// Some marker files don't represent buildable projects. For example,
    /// Elixir umbrella roots have a mix.exs but aren't projects themselves -
    /// only their child apps are projects.
    ///
    /// Returns true if this config file should be skipped.
    fn should_skip(&self, _config_path: &Path) -> bool {
        false
    }

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

    /// Choose what a files-list target runs for a set of changed files.
    ///
    /// Plugins that need to read the project (for example to follow imports)
    /// override this. The default delegates to [`Self::with_files_list`]: a
    /// modified command runs instead of the original, and `None` becomes
    /// [`FilesListPlan::Declined`].
    ///
    /// - project_dir: absolute project directory
    /// - files: changed file paths relative to `project_dir`
    fn select_for_files(
        &self,
        _project_dir: &Path,
        target_name: &str,
        command: &str,
        files: &[PathBuf],
    ) -> FilesListSelection {
        match self.with_files_list(target_name, command, files) {
            Some(command) => FilesListSelection::new(FilesListPlan::Commands(vec![command])),
            None => FilesListSelection::new(FilesListPlan::Declined),
        }
    }

    /// Narrow a test command to the tests selected by
    /// `aster affected --related`.
    ///
    /// Returns `None` when `command` is not a test command the plugin can
    /// narrow, in which case it runs as written. [`FilesListPlan::Nothing`]
    /// means the command would run none of the selected tests.
    ///
    /// - project_dir: absolute project directory
    /// - tests: selected test files of this project's language
    fn related_tests(
        &self,
        _project_dir: &Path,
        _command: &str,
        _tests: &[RelatedTest],
    ) -> Option<FilesListPlan> {
        None
    }

    /// [`Self::related_tests`] for a command run by a shell one-liner that
    /// may change its environment where Aster cannot read it (`export`,
    /// `source env.sh`). A plugin whose narrowing reads the environment
    /// overrides this to stay correct without it.
    fn related_tests_in_hidden_environment(
        &self,
        project_dir: &Path,
        command: &str,
        tests: &[RelatedTest],
    ) -> Option<FilesListPlan> {
        self.related_tests(project_dir, command, tests)
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

    /// Project-relative output globs the artifact cache stores for this target
    /// when the user did not set `[targets.<name>.cache] outputs`.
    ///
    /// Globs are expanded by the outputs packer. An empty list stores no
    /// artifacts. Log-only targets (`test`, `lint`, `typecheck`, `check`,
    /// `format`) return none. A user-set `outputs` list, including an empty
    /// one, replaces this default.
    ///
    /// - target_name: name of the target (e.g., "build", "release")
    /// - command: the effective command after aster.toml overrides
    /// - ctx: project paths and the native config, used to read the build script
    fn default_outputs(
        &self,
        _target_name: &str,
        _command: &str,
        _ctx: &TargetContext,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Workspace-root files that belong in this project's cache key.
    ///
    /// `cache_inputs` has no project path, so it cannot tell a workspace
    /// member from a standalone project. Plugins that share a root lockfile
    /// or config override this and return workspace-relative patterns.
    /// The default is none: a project that is its own workspace root must
    /// not hash `Cargo.lock` a second time under the `ws:` prefix.
    ///
    /// - project_root: the project directory
    /// - workspace_root: the Aster workspace root
    /// - target_name: name of the target (e.g., "test", "deps")
    fn workspace_cache_inputs(
        &self,
        _project_root: &Path,
        _workspace_root: &Path,
        _target_name: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Warm-start directories for this language's native tools.
    ///
    /// Default is empty: a plugin opts in by describing the directories
    /// actions/cache should restore before a run. This does not change
    /// what a run executes.
    fn native_caches(&self, _ctx: &TargetContext) -> Vec<NativeCache> {
        Vec::new()
    }

    /// Plan how to capture per-test results from one of a target's commands.
    ///
    /// Called only when test capture is on, after the target's cache key was
    /// computed from the command as written. Return `None` when `ctx.command`
    /// is not a test run the plugin can capture; it then runs unchanged. A
    /// plan must leave the command's console output as it was.
    fn test_report_plan(&self, _ctx: &ReportContext) -> Option<ReportPlan> {
        None
    }

    /// Provide the default clean target for this language
    ///
    /// Returns None if the language doesn't have a standard clean operation.
    /// The returned target should have `invalidates_cache: true` set.
    ///
    /// - ctx: Target context with project paths and dependencies
    fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
        None
    }
}

pub(crate) mod capture_audit;
pub mod elixir;
pub mod go;
pub mod gradle;
mod jvm;
pub mod maven;
pub(crate) mod mix_partitions;
pub mod nodejs;
pub mod python;
pub mod registry;
pub(crate) mod related_tests;
pub mod ruby;
pub mod rust;
pub(crate) mod rust_related;
pub(crate) mod test_reports;
pub(crate) mod test_retry;

pub use elixir::ElixirPlugin;
pub use go::GoPlugin;
pub use gradle::GradlePlugin;
pub use maven::MavenPlugin;
pub use nodejs::NodeJsPlugin;
pub use python::PythonPlugin;
pub use registry::PluginRegistry;
pub use ruby::RubyPlugin;
pub use rust::RustPlugin;

// Re-export CacheInputs for use by cache module
pub use self::CacheInputs as PluginCacheInputs;

#[cfg(test)]
mod tests {
    use super::PluginRegistry;

    /// Default target names plugins may return cache inputs for, including
    /// the implicit clean target and the non-cacheable dev target.
    const DEFAULT_TARGET_NAMES: &[&str] = &["build", "test", "lint", "deps", "dev", "clean"];

    #[test]
    fn test_no_plugin_tracks_ci_env_var() {
        let registry = PluginRegistry::with_all_plugins();
        assert!(
            !registry.plugins().is_empty(),
            "with_all_plugins registered nothing"
        );

        for plugin in registry.plugins() {
            for target_name in DEFAULT_TARGET_NAMES {
                let inputs = plugin.cache_inputs(target_name);
                assert!(
                    !inputs.env_vars.iter().any(|var| var == "CI"),
                    "{} cache_inputs({target_name}) tracks CI: {:?}",
                    plugin.name(),
                    inputs.env_vars
                );
            }
        }
    }

    #[test]
    fn inferred_languages_hash_every_project_file_except_deps() {
        let registry = PluginRegistry::with_all_plugins();
        for name in ["go", "nodejs", "elixir", "rust", "python"] {
            let plugin = registry
                .find_by_name(name)
                .unwrap_or_else(|| panic!("{name} plugin missing"));
            assert!(
                plugin.cache_inputs("deps").source_globs.is_empty(),
                "{name} deps must stay lockfile-only"
            );
            for target_name in ["build", "test", "lint", "format", "check"] {
                assert_eq!(
                    plugin.cache_inputs(target_name).source_globs,
                    vec![super::PROJECT_FILES_GLOB.to_string()],
                    "{name} cache_inputs({target_name})"
                );
            }
        }
    }
}
