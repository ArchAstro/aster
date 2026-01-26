//! aster.toml configuration parsing
//!
//! Each project can have an optional aster.toml file that:
//! - Overrides the project name (instead of inferring from native config)
//! - Adds cross-language dependencies via depends_on
//! - Defines custom targets

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::address::Address;

/// Configuration from an aster.toml file
#[derive(Debug, Clone, Default, Deserialize)]
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
}

/// Target configuration - supports both simple command string and rich format
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TargetConfig {
    /// Simple format: just a command string
    /// e.g., `lint = "npm run lint"`
    Simple(String),

    /// Rich format with full configuration
    /// e.g., `[targets.test]` with command, depends_on, etc.
    Rich(RichTargetConfig),
}

/// Rich target configuration with all options
#[derive(Debug, Clone, Default, Deserialize)]
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
}

impl TargetConfig {
    /// Get the command string
    pub fn command(&self) -> &str {
        match self {
            TargetConfig::Simple(cmd) => cmd,
            TargetConfig::Rich(rich) => &rich.command,
        }
    }

    /// Get depends_on (empty for simple format)
    pub fn depends_on(&self) -> &[String] {
        match self {
            TargetConfig::Simple(_) => &[],
            TargetConfig::Rich(rich) => &rich.depends_on,
        }
    }

    /// Get capabilities (empty for simple format)
    pub fn capabilities(&self) -> &[String] {
        match self {
            TargetConfig::Simple(_) => &[],
            TargetConfig::Rich(rich) => &rich.capabilities,
        }
    }

    /// Get files_glob (None for simple format)
    pub fn files_glob(&self) -> Option<&str> {
        match self {
            TargetConfig::Simple(_) => None,
            TargetConfig::Rich(rich) => rich.files_glob.as_deref(),
        }
    }

    /// Get stream flag (false for simple format)
    pub fn stream(&self) -> bool {
        match self {
            TargetConfig::Simple(_) => false,
            TargetConfig::Rich(rich) => rich.stream,
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

    // Validate depends_on entries are valid addresses
    for dep in &config.depends_on {
        Address::parse(dep)
            .with_context(|| format!("Invalid dependency '{}' in {}", dep, path.display()))?;
    }

    Ok(config)
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
}
