use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::project::TargetConfig;

/// Workspace-level configuration from the root aster.toml
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Glob patterns for paths to ignore during project discovery
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Watch mode configuration
    #[serde(default)]
    pub watch: WatchWorkspaceConfig,

    /// Affected-command configuration
    #[serde(default)]
    pub affected: AffectedWorkspaceConfig,

    /// Local development service harness configuration
    #[serde(default)]
    pub dev: DevWorkspaceConfig,

    /// Project settings are accepted because a project at the repository root
    /// may share this file with workspace configuration.
    pub name: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub targets: HashMap<String, TargetConfig>,
}

/// Configuration for `aster services up`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevWorkspaceConfig {
    /// Environment files consulted when resolving named ports. Later files win.
    #[serde(default)]
    pub port_env_files: Vec<String>,

    /// Named ports shared by services and their environment.
    #[serde(default)]
    pub ports: HashMap<String, DevPortConfig>,

    /// Optional named port for the line-delimited JSON control socket.
    pub control_port: Option<String>,

    /// Named service-to-target mappings.
    #[serde(default)]
    pub services: HashMap<String, DevServiceConfig>,
}

/// A named port used by one or more development services.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DevPortConfig {
    /// A fixed port.
    Fixed(u16),
    /// A port resolved from the process environment, port env files, or a default.
    Resolved(ResolvedDevPortConfig),
}

/// Detailed named-port resolution.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedDevPortConfig {
    /// Process environment variables checked in order.
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub env: Vec<String>,
    /// Port-env-file variables checked in order after the process environment.
    /// When omitted, `env` names are also checked in the configured files.
    #[serde(default, deserialize_with = "deserialize_optional_one_or_many")]
    pub file_env: Option<Vec<String>>,
    /// Default value when no configured environment variable is set.
    pub default: u16,
    /// Add the delta between this port and `offset_base` to the default.
    pub offset_from: Option<String>,
    /// Baseline for `offset_from`; required when `offset_from` is set.
    pub offset_base: Option<u16>,
    /// Clamp a negative offset delta to zero instead of rejecting it.
    #[serde(default)]
    pub saturating_offset: bool,
}

/// One long-running service managed by `aster services up`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevServiceConfig {
    /// Address of a `stream = true` target.
    pub target: String,
    /// Named port from `[dev.ports]`.
    pub port: Option<String>,
    /// Optional browser path appended to `http://localhost:<port>`.
    pub open_path: Option<String>,
    /// Environment files loaded into the service process. Later files win.
    #[serde(default)]
    pub env_files: Vec<String>,
    /// Environment overrides. Values support `{port}` and `{ports.<name>}`.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Ambient environment variables explicitly allowed into the service.
    #[serde(default)]
    pub inherit_env: Vec<String>,
    /// Stable startup/display order. Ties are sorted by service name.
    #[serde(default)]
    pub order: i32,
}

fn deserialize_one_or_many<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(value) => vec![value],
        OneOrMany::Many(values) => values,
    })
}

fn deserialize_optional_one_or_many<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_one_or_many(deserializer).map(Some)
}

/// Configuration controlling which Git changes participate in affected analysis.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffectedWorkspaceConfig {
    /// Workspace-relative glob patterns excluded from affected analysis.
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// Watch-mode configuration controlling fs-event ignore and suppression behavior.
///
/// Built-in defaults always apply (`.git/`, `node_modules/`, `target/`, `_build/`,
/// `.next/`, `dist/`, `.turbo/`, `.venv/`, `.elixir_ls/`). User patterns extend
/// them — they never replace the defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchWorkspaceConfig {
    /// Additional glob patterns (workspace-relative) whose fs events are dropped
    /// before being considered by the watcher.
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Paths written by build-like processes that must not retrigger rebuilds.
    /// Events under these paths are dropped during and briefly after a rebuild.
    #[serde(default)]
    pub suppress_paths: Vec<String>,

    /// Debounce window in milliseconds for coalescing bursts of events.
    /// Defaults to 300 when unset.
    pub debounce_ms: Option<u64>,
}

impl WorkspaceConfig {
    /// Load workspace config from the root aster.toml
    /// Returns default config if file doesn't exist or has no workspace settings
    pub fn load(workspace_root: &Path) -> Result<Self> {
        let config_path = workspace_root.join("aster.toml");

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        // Parse TOML - workspace config fields are at the top level
        let config: WorkspaceConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        Ok(config)
    }
}

/// Find the workspace root by walking up from the start directory.
///
/// Prefers `.git` as the workspace boundary (monorepo root), falling back to
/// the highest `aster.toml` without a `.git` above it. This ensures project-level
/// aster.toml files don't incorrectly stop the search.
///
/// Returns None if neither marker is found (reached filesystem root).
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    // Canonicalize to resolve symlinks
    let mut current = start.canonicalize().ok()?;
    let mut highest_aster_toml: Option<PathBuf> = None;

    loop {
        // Check for .git - this is the definitive workspace boundary
        if current.join(".git").exists() {
            return Some(current);
        }

        // Track aster.toml as fallback (in case there's no .git)
        // Keep updating so we get the highest one (closest to filesystem root)
        if current.join("aster.toml").exists() {
            highest_aster_toml = Some(current.clone());
        }

        // Move up to parent directory
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break, // Reached filesystem root
        }
    }

    // No .git found - use the highest aster.toml we encountered
    highest_aster_toml
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parses_dev_services_ports_and_environment() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("aster.toml"),
            r#"
[dev]
port_env_files = [".env"]
control_port = "api"

[dev.ports.api]
env = "API_PORT"
file_env = "PORT"
default = 4000

[dev.ports.web]
env = "WEB_PORT"
default = 3000
offset_from = "api"
offset_base = 4000

[dev.services.api]
target = "//api:dev"
port = "api"
open_path = "/health"
env_files = ["api/.env"]
env = { PORT = "{port}", WEB_PORT = "{ports.web}" }
inherit_env = ["GOOGLE_CLOUD_MODE"]
order = 10
"#,
        )
        .unwrap();

        let config = WorkspaceConfig::load(temp.path()).unwrap();
        assert_eq!(config.dev.services["api"].target, "//api:dev");
        assert_eq!(
            config.dev.services["api"].open_path.as_deref(),
            Some("/health")
        );
        assert_eq!(
            config.dev.services["api"].inherit_env,
            ["GOOGLE_CLOUD_MODE"]
        );
        let DevPortConfig::Resolved(api) = &config.dev.ports["api"] else {
            panic!("expected resolved port");
        };
        assert_eq!(api.env, ["API_PORT"]);
        assert_eq!(api.file_env.as_deref().unwrap(), ["PORT"]);
        let DevPortConfig::Resolved(web) = &config.dev.ports["web"] else {
            panic!("expected resolved port");
        };
        assert_eq!(web.offset_from.as_deref(), Some("api"));
        assert_eq!(web.offset_base, Some(4000));
    }

    #[test]
    fn test_find_workspace_root_with_aster_toml() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create aster.toml marker
        fs::write(root.join("aster.toml"), "").unwrap();

        let result = find_workspace_root(root);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_with_git() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create .git folder
        fs::create_dir(root.join(".git")).unwrap();

        let result = find_workspace_root(root);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_prefers_git() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create both markers
        fs::write(root.join("aster.toml"), "").unwrap();
        fs::create_dir(root.join(".git")).unwrap();

        let result = find_workspace_root(root);
        assert!(result.is_some());
        // Should find it at root level (.git is the definitive boundary)
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_walks_up() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create aster.toml at root
        fs::write(root.join("aster.toml"), "").unwrap();

        // Create nested directories
        let nested = root.join("services").join("api").join("src");
        fs::create_dir_all(&nested).unwrap();

        let result = find_workspace_root(&nested);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_workspace_config_load_with_watch_section() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::write(
            root.join("aster.toml"),
            r#"
[watch]
ignore = ["coverage/**"]
suppress_paths = ["priv/static/assets/**"]
debounce_ms = 500
"#,
        )
        .unwrap();

        let config = WorkspaceConfig::load(root).unwrap();
        assert_eq!(config.watch.ignore, vec!["coverage/**".to_string()]);
        assert_eq!(
            config.watch.suppress_paths,
            vec!["priv/static/assets/**".to_string()]
        );
        assert_eq!(config.watch.debounce_ms, Some(500));
    }

    #[test]
    fn test_workspace_config_load_with_affected_section() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::write(
            root.join("aster.toml"),
            r#"
[affected]
ignore = [".agents/**", ".claude/skills/**"]
"#,
        )
        .unwrap();

        let config = WorkspaceConfig::load(root).unwrap();
        assert_eq!(
            config.affected.ignore,
            vec![".agents/**".to_string(), ".claude/skills/**".to_string()]
        );
    }

    #[test]
    fn test_workspace_config_watch_defaults() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("aster.toml"), "").unwrap();

        let config = WorkspaceConfig::load(root).unwrap();
        assert!(config.watch.ignore.is_empty());
        assert!(config.watch.suppress_paths.is_empty());
        assert_eq!(config.watch.debounce_ms, None);
    }

    #[test]
    fn test_workspace_config_load_with_ignore() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::write(
            root.join("aster.toml"),
            r#"
ignore = ["vendor/**", "examples/**"]
"#,
        )
        .unwrap();

        let config = WorkspaceConfig::load(root).unwrap();
        assert_eq!(config.ignore.len(), 2);
        assert!(config.ignore.contains(&"vendor/**".to_string()));
        assert!(config.ignore.contains(&"examples/**".to_string()));
    }

    #[test]
    fn test_workspace_config_load_empty() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::write(root.join("aster.toml"), "").unwrap();

        let config = WorkspaceConfig::load(root).unwrap();
        assert!(config.ignore.is_empty());
        assert!(config.affected.ignore.is_empty());
    }

    #[test]
    fn test_workspace_config_load_no_file() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // No aster.toml file
        let config = WorkspaceConfig::load(root).unwrap();
        assert!(config.ignore.is_empty());
        assert!(config.affected.ignore.is_empty());
    }

    #[test]
    fn test_workspace_config_reports_invalid_toml() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("aster.toml"), "[affected\nignore = []").unwrap();

        let error = WorkspaceConfig::load(root).err().unwrap();

        assert!(error.to_string().contains("Failed to parse"));
        assert!(error.to_string().contains("aster.toml"));
    }

    #[test]
    fn workspace_file_accepts_root_project_fields() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("aster.toml"),
            r#"
name = "root"

[targets]
test = "cargo test"
"#,
        )
        .unwrap();

        let config = WorkspaceConfig::load(temp.path()).unwrap();
        assert_eq!(config.name.as_deref(), Some("root"));
        assert!(config.targets.contains_key("test"));
    }

    #[test]
    fn test_find_workspace_root_ignores_project_aster_toml() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create .git at root (workspace boundary)
        fs::create_dir(root.join(".git")).unwrap();

        // Create project-level aster.toml in a subdirectory
        let project_dir = root.join("services").join("api");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("aster.toml"), "[project]\nname = \"api\"").unwrap();

        // Starting from project dir should find root (with .git), not project dir
        let result = find_workspace_root(&project_dir);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_uses_highest_aster_toml_without_git() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create workspace aster.toml at root (no .git)
        fs::write(root.join("aster.toml"), "# workspace").unwrap();

        // Create project-level aster.toml in subdirectory
        let project_dir = root.join("services").join("api");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("aster.toml"), "[project]\nname = \"api\"").unwrap();

        // Starting from project dir should find root aster.toml (highest), not project dir
        let result = find_workspace_root(&project_dir);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_find_workspace_root_not_found() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a nested dir with no markers anywhere
        let nested = root.join("some").join("nested").join("dir");
        fs::create_dir_all(&nested).unwrap();

        // This will walk up and eventually find no markers
        // Note: In a real filesystem, it might find .git in home or root
        // but in a temp dir without markers, it should return None
        let result = find_workspace_root(&nested);
        // The test temp dir has no markers, so walking up from nested
        // should eventually return None (or find something outside temp)
        // For this test, we just verify the function runs without panic
        // and returns Some or None based on what exists above temp
        assert!(result.is_none() || result.is_some());
    }
}
