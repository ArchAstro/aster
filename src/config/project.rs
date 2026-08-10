//! aster.toml configuration parsing
//!
//! Each project can have an optional aster.toml file that:
//! - Overrides the project name (instead of inferring from native config)
//! - Adds cross-language dependencies via depends_on
//! - Defines custom targets

use anyhow::{Context, Result};
use globset::Glob;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::workspace::{AffectedWorkspaceConfig, WatchWorkspaceConfig};
use crate::address::Address;

/// Configuration from an aster.toml file
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsterToml {
    /// Override project name (instead of inferring from native config)
    pub name: Option<String>,

    /// Cross-language dependencies: ["//services/platform:build"]
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Custom targets - supports both simple and rich formats:
    /// Simple: `lint = "npm run lint"`
    /// Rich: `[targets.test]` with command, depends_on, capabilities, files_glob
    #[serde(default)]
    pub targets: HashMap<String, TargetConfig>,

    /// Workspace discovery settings are accepted because a repository-root
    /// project may share this file with workspace configuration.
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub watch: WatchWorkspaceConfig,
    #[serde(default)]
    pub affected: AffectedWorkspaceConfig,
    #[serde(default)]
    pub dev: super::workspace::DevWorkspaceConfig,
}

/// Alias target configuration - references another target in the same project
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasTargetConfig {
    /// The target name to alias (bare name or //self:name)
    pub alias: String,

    /// Additional dependencies appended to the aliased target's deps
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// Target configuration - supports simple command string, alias, and rich format
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TargetConfig {
    /// Simple format: just a command string
    /// e.g., `lint = "npm run lint"`
    Simple(String),

    /// Alias format: references another target in the same project
    /// e.g., `check = { alias = "test" }`
    Alias(AliasTargetConfig),

    /// Rich format with full configuration
    /// e.g., `[targets.test]` with command, depends_on, etc.
    Rich(RichTargetConfig),
}

/// Cache configuration for a target
///
/// Allows users to customize which files and environment variables
/// are included in the cache key for a target.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Explicitly enable or disable memoization for this target.
    pub enabled: Option<bool>,
    /// Additional files/globs to include in cache key
    #[serde(default)]
    pub include: Vec<String>,
    /// Files/globs to exclude from cache key
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Additional environment variables to track
    #[serde(default)]
    pub env: Vec<String>,
    /// Output paths that must exist before a cached success may be reused.
    #[serde(default)]
    pub outputs: Vec<String>,
}

/// Rich target configuration with all options
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RichTargetConfig {
    /// The command to execute (may contain {files} placeholder)
    pub command: String,

    /// Target dependencies: ["//self:deps", "//libs/shared:build"]
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Capabilities: ["files_list"]
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Glob pattern to filter files for files_list capability
    /// e.g., "*_test.py" or "*.spec.ts"
    pub files_glob: Option<String>,

    /// Stream output to stdout in real-time (for long-running processes like dev servers)
    #[serde(default)]
    pub stream: bool,

    /// Cache configuration overrides
    #[serde(default)]
    pub cache: Option<CacheConfig>,

    /// Invalidate all project cache entries after successful execution
    #[serde(default)]
    pub invalidates_cache: bool,

    /// Named resources requiring exclusive access within a DAG level
    #[serde(default)]
    pub exclusive_resources: Vec<String>,
}

impl TargetConfig {
    /// Get the command string (empty for alias — resolved later)
    pub fn command(&self) -> &str {
        match self {
            TargetConfig::Simple(cmd) => cmd,
            TargetConfig::Alias(_) => "",
            TargetConfig::Rich(rich) => &rich.command,
        }
    }

    /// Get depends_on (empty for simple/alias format — alias deps resolved later)
    pub fn depends_on(&self) -> &[String] {
        match self {
            TargetConfig::Simple(_) => &[],
            TargetConfig::Alias(alias) => &alias.depends_on,
            TargetConfig::Rich(rich) => &rich.depends_on,
        }
    }

    /// Get capabilities (empty for simple/alias format)
    pub fn capabilities(&self) -> &[String] {
        match self {
            TargetConfig::Simple(_) | TargetConfig::Alias(_) => &[],
            TargetConfig::Rich(rich) => &rich.capabilities,
        }
    }

    /// Get files_glob (None for simple/alias format)
    pub fn files_glob(&self) -> Option<&str> {
        match self {
            TargetConfig::Simple(_) | TargetConfig::Alias(_) => None,
            TargetConfig::Rich(rich) => rich.files_glob.as_deref(),
        }
    }

    /// Get stream flag (false for simple/alias format)
    pub fn stream(&self) -> bool {
        match self {
            TargetConfig::Simple(_) | TargetConfig::Alias(_) => false,
            TargetConfig::Rich(rich) => rich.stream,
        }
    }

    /// Get cache configuration (None for simple/alias format)
    pub fn cache(&self) -> Option<&CacheConfig> {
        match self {
            TargetConfig::Simple(_) | TargetConfig::Alias(_) => None,
            TargetConfig::Rich(rich) => rich.cache.as_ref(),
        }
    }

    /// Get invalidates_cache flag (false for simple/alias format)
    pub fn invalidates_cache(&self) -> bool {
        match self {
            TargetConfig::Simple(_) | TargetConfig::Alias(_) => false,
            TargetConfig::Rich(rich) => rich.invalidates_cache,
        }
    }

    /// Get exclusive_resources (empty for simple/alias format)
    pub fn exclusive_resources(&self) -> &[String] {
        match self {
            TargetConfig::Simple(_) | TargetConfig::Alias(_) => &[],
            TargetConfig::Rich(rich) => &rich.exclusive_resources,
        }
    }

    /// Check if this is an alias target
    pub fn is_alias(&self) -> bool {
        matches!(self, TargetConfig::Alias(_))
    }

    /// Get the alias target name, if this is an alias
    pub fn alias_target(&self) -> Option<&str> {
        match self {
            TargetConfig::Alias(alias) => Some(&alias.alias),
            _ => None,
        }
    }
}

/// Parse an aster.toml file from the given path
///
/// Validates that all depends_on entries are valid addresses.
pub fn parse_aster_toml(path: &Path) -> Result<AsterToml> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let config: AsterToml =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;

    validate_aster_config(&config.depends_on, &config.targets, path)?;
    config
        .dev
        .validate()
        .with_context(|| format!("Invalid service groups in {}", path.display()))?;

    Ok(config)
}

/// Validate fields shared by the project and workspace views of aster.toml.
///
/// The root file is deserialized through both views by different commands, so
/// semantic validity must not depend on which command happened to load it.
pub(super) fn validate_aster_config(
    depends_on: &[String],
    targets: &HashMap<String, TargetConfig>,
    path: &Path,
) -> Result<()> {
    for dep in depends_on {
        Address::parse(dep)
            .with_context(|| format!("Invalid dependency '{}' in {}", dep, path.display()))?;
    }

    for (target_name, target) in targets {
        for dep in target.depends_on() {
            Address::parse(dep).with_context(|| {
                format!(
                    "Invalid dependency '{dep}' for target '{target_name}' in {}",
                    path.display()
                )
            })?;
        }

        let TargetConfig::Rich(target) = target else {
            continue;
        };

        for capability in &target.capabilities {
            if capability != "files_list" {
                anyhow::bail!(
                    "Unsupported capability '{capability}' for target '{target_name}' in {}. \
                     Supported capabilities: files_list",
                    path.display()
                );
            }
        }

        if let Some(pattern) = &target.files_glob {
            Glob::new(pattern).with_context(|| {
                format!(
                    "Invalid files_glob '{pattern}' for target '{target_name}' in {}",
                    path.display()
                )
            })?;
        }

        if let Some(cache) = &target.cache {
            for pattern in cache.include.iter().chain(&cache.exclude) {
                Glob::new(pattern).with_context(|| {
                    format!(
                        "Invalid cache glob '{pattern}' for target '{target_name}' in {}",
                        path.display()
                    )
                })?;
            }
        }
    }

    Ok(())
}

/// Check if an aster.toml file exists in the given project directory
///
/// Returns Some(path) if aster.toml exists, None otherwise.
pub fn find_aster_toml(project_dir: &Path) -> Option<PathBuf> {
    let path = project_dir.join("aster.toml");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_aster_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(&toml_path, r#"name = "my-custom-name""#).unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.name, Some("my-custom-name".to_string()));
        assert!(config.depends_on.is_empty());
        assert!(config.targets.is_empty());
    }

    #[test]
    fn test_parse_depends_on() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
depends_on = [
    "//services/platform:build",
    "//libs/shared"
]
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.depends_on.len(), 2);
        assert_eq!(config.depends_on[0], "//services/platform:build");
        assert_eq!(config.depends_on[1], "//libs/shared");
    }

    #[test]
    fn test_parse_targets_simple() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets]
lint = "npm run eslint"
typecheck = "tsc --noEmit"
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.targets.len(), 2);
        assert_eq!(
            config.targets.get("lint").map(|t| t.command()),
            Some("npm run eslint")
        );
        assert_eq!(
            config.targets.get("typecheck").map(|t| t.command()),
            Some("tsc --noEmit")
        );
        // Simple format has empty depends_on and capabilities
        assert!(config.targets.get("lint").unwrap().depends_on().is_empty());
    }

    #[test]
    fn test_parse_targets_rich() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.test]
command = "pytest {files}"
depends_on = ["//self:deps", "//libs/shared:build"]
capabilities = ["files_list"]
files_glob = "*_test.py"

[targets.build]
command = "python -m build"
depends_on = ["//self:deps"]
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.targets.len(), 2);

        let test_target = config.targets.get("test").unwrap();
        assert_eq!(test_target.command(), "pytest {files}");
        assert_eq!(
            test_target.depends_on(),
            &["//self:deps", "//libs/shared:build"]
        );
        assert_eq!(test_target.capabilities(), &["files_list"]);
        assert_eq!(test_target.files_glob(), Some("*_test.py"));

        let build_target = config.targets.get("build").unwrap();
        assert_eq!(build_target.command(), "python -m build");
        assert_eq!(build_target.depends_on(), &["//self:deps"]);
        assert!(build_target.capabilities().is_empty());
        assert_eq!(build_target.files_glob(), None);
    }

    #[test]
    fn rejects_unknown_capability_and_invalid_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.test]
command = "pytest {files}"
capabilities = ["FilesList"]
"#,
        )
        .unwrap();
        let error = parse_aster_toml(&toml_path).unwrap_err();
        assert!(error.to_string().contains("Unsupported capability"));

        std::fs::write(
            &toml_path,
            r#"
[targets.test]
command = "pytest {files}"
capabilities = ["files_list"]
files_glob = "["
"#,
        )
        .unwrap();
        let error = parse_aster_toml(&toml_path).unwrap_err();
        assert!(error.to_string().contains("Invalid files_glob"));

        std::fs::write(
            &toml_path,
            r#"
[targets.test]
command = "pytest"

[targets.test.cache]
include = ["["]
"#,
        )
        .unwrap();
        let error = parse_aster_toml(&toml_path).unwrap_err();
        assert!(error.to_string().contains("Invalid cache glob"));
    }

    #[test]
    fn rejects_unknown_rich_target_and_cache_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.test]
command = "pytest"
unknown = true
"#,
        )
        .unwrap();
        assert!(parse_aster_toml(&toml_path).is_err());

        std::fs::write(
            &toml_path,
            r#"
[targets.test]
command = "pytest"

[targets.test.cache]
enabled = true
artifact = "report.xml"
"#,
        )
        .unwrap();
        assert!(parse_aster_toml(&toml_path).is_err());
    }

    #[test]
    fn root_file_accepts_workspace_fields_but_rejects_unknown_top_level_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
ignore = ["vendor/**"]

[watch]
debounce_ms = 250
"#,
        )
        .unwrap();
        assert!(parse_aster_toml(&toml_path).is_ok());

        std::fs::write(&toml_path, "mystery = true").unwrap();
        assert!(parse_aster_toml(&toml_path).is_err());
    }

    #[test]
    fn test_parse_targets_mixed() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets]
lint = "npm run lint"

[targets.test]
command = "npm test {files}"
capabilities = ["files_list"]
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.targets.len(), 2);
        assert_eq!(
            config.targets.get("lint").map(|t| t.command()),
            Some("npm run lint")
        );
        assert_eq!(
            config.targets.get("test").map(|t| t.command()),
            Some("npm test {files}")
        );
    }

    #[test]
    fn test_parse_full_aster_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
name = "api-service"
depends_on = ["//libs/auth:build", "//libs/db"]

[targets]
build = "npm run build"
test = "npm test"
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.name, Some("api-service".to_string()));
        assert_eq!(config.depends_on.len(), 2);
        assert_eq!(config.targets.len(), 2);
    }

    #[test]
    fn test_parse_target_with_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.run-dev]
command = "poetry run scrape-server"
stream = true

[targets.test]
command = "pytest"
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert_eq!(config.targets.len(), 2);
        // run-dev should have stream=true
        let run_dev = config.targets.get("run-dev").unwrap();
        assert_eq!(run_dev.command(), "poetry run scrape-server");
        assert!(run_dev.stream());
        // test should have stream=false (default)
        let test = config.targets.get("test").unwrap();
        assert_eq!(test.command(), "pytest");
        assert!(!test.stream());
    }

    #[test]
    fn test_parse_alias_bare_name() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets]
check = { alias = "test" }
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        let check = config.targets.get("check").unwrap();
        assert!(check.is_alias());
        assert_eq!(check.alias_target(), Some("test"));
        assert!(check.depends_on().is_empty());
        // Alias returns defaults for other accessors
        assert_eq!(check.command(), "");
    }

    #[test]
    fn test_parse_alias_with_self_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets]
check = { alias = "//self:test" }
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        let check = config.targets.get("check").unwrap();
        assert!(check.is_alias());
        assert_eq!(check.alias_target(), Some("//self:test"));
    }

    #[test]
    fn test_parse_alias_with_extra_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.ci]
alias = "test"
depends_on = ["//self:lint", "//self:typecheck"]
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        let ci = config.targets.get("ci").unwrap();
        assert!(ci.is_alias());
        assert_eq!(ci.alias_target(), Some("test"));
        assert_eq!(ci.depends_on(), &["//self:lint", "//self:typecheck"]);
    }

    #[test]
    fn test_parse_invalid_address() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(&toml_path, r#"depends_on = ["invalid-address-no-slashes"]"#).unwrap();

        let result = parse_aster_toml(&toml_path);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid dependency"));
    }

    #[test]
    fn test_find_aster_toml_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(&toml_path, "").unwrap();

        let result = find_aster_toml(tmp.path());

        assert!(result.is_some());
        assert_eq!(result.unwrap(), toml_path);
    }

    #[test]
    fn test_find_aster_toml_missing() {
        let tmp = tempfile::tempdir().unwrap();

        let result = find_aster_toml(tmp.path());

        assert!(result.is_none());
    }

    #[test]
    fn test_parse_empty_aster_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(&toml_path, "").unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        assert!(config.name.is_none());
        assert!(config.depends_on.is_empty());
        assert!(config.targets.is_empty());
    }

    #[test]
    fn test_parse_exclusive_resources() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.deps]
command = "mix deps.get"
exclusive_resources = ["hex_registry"]

[targets.build]
command = "mix compile"
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        let deps = config.targets.get("deps").unwrap();
        assert_eq!(deps.exclusive_resources(), &["hex_registry"]);

        let build = config.targets.get("build").unwrap();
        assert!(build.exclusive_resources().is_empty());
    }

    #[test]
    fn test_exclusive_resources_defaults_empty_for_simple() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets]
test = "npm test"
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        let test = config.targets.get("test").unwrap();
        assert!(test.exclusive_resources().is_empty());
    }

    #[test]
    fn test_parse_target_with_invalidates_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"
[targets.clean]
command = "rm -rf node_modules"
invalidates_cache = true

[targets.build]
command = "npm run build"
"#,
        )
        .unwrap();

        let config = parse_aster_toml(&toml_path).unwrap();

        let clean = config.targets.get("clean").unwrap();
        assert_eq!(clean.command(), "rm -rf node_modules");
        assert!(clean.invalidates_cache());

        let build = config.targets.get("build").unwrap();
        assert_eq!(build.command(), "npm run build");
        assert!(!build.invalidates_cache());
    }
}
