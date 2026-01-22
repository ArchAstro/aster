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

    /// Custom targets: { "lint": "npm run eslint", "typecheck": "tsc --noEmit" }
    #[serde(default)]
    pub targets: HashMap<String, String>,
}

/// Parse an aster.toml file from the given path
///
/// Validates that all depends_on entries are valid addresses.
pub fn parse_aster_toml(path: &Path) -> Result<AsterToml> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let config: AsterToml = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

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
    fn test_parse_targets() {
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
        assert_eq!(config.targets.get("lint"), Some(&"npm run eslint".to_string()));
        assert_eq!(config.targets.get("typecheck"), Some(&"tsc --noEmit".to_string()));
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
    fn test_parse_invalid_address() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("aster.toml");
        std::fs::write(
            &toml_path,
            r#"depends_on = ["invalid-address-no-slashes"]"#,
        )
        .unwrap();

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
