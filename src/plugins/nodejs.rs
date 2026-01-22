//! Node.js language plugin for package.json parsing

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use super::{LanguagePlugin, LocalDependency, ProjectMetadata};

/// Internal representation of package.json for serde deserialization
#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
}

/// Node.js plugin for discovering and parsing npm/yarn/pnpm projects
pub struct NodeJsPlugin;

impl LanguagePlugin for NodeJsPlugin {
    fn name(&self) -> &str {
        "nodejs"
    }

    fn marker_files(&self) -> &[&str] {
        &["package.json"]
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pkg: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let name = pkg
            .name
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow!("Missing or empty 'name' field in {}", config_path.display()))?;

        Ok(ProjectMetadata {
            name,
            version: pkg.version,
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pkg: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let project_dir = config_path
            .parent()
            .ok_or_else(|| anyhow!("Config path has no parent directory"))?;

        let mut deps = Vec::new();

        // Extract file: dependencies from both dependencies and devDependencies
        for dep_map in [pkg.dependencies, pkg.dev_dependencies].into_iter().flatten() {
            for (name, version) in dep_map {
                if let Some(path_str) = version.strip_prefix("file:") {
                    let path = project_dir.join(path_str);
                    deps.push(LocalDependency {
                        name,
                        path,
                    });
                }
            }
        }

        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "version": "1.2.3"}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let metadata = plugin.parse_project(tmp.path(), &pkg_json).unwrap();

        assert_eq!(metadata.name, "my-app");
        assert_eq!(metadata.version, Some("1.2.3".to_string()));
    }

    #[test]
    fn test_parse_file_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
                "name": "my-app",
                "dependencies": {
                    "shared": "file:../shared"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&pkg_json).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "shared");
        assert_eq!(deps[0].path, tmp.path().join("../shared"));
    }

    #[test]
    fn test_parse_file_dev_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
                "name": "my-app",
                "devDependencies": {
                    "test-utils": "file:../test-utils"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&pkg_json).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "test-utils");
        assert_eq!(deps[0].path, tmp.path().join("../test-utils"));
    }

    #[test]
    fn test_parse_mixed_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
                "name": "my-app",
                "dependencies": {
                    "lodash": "^4.17.21",
                    "local-lib": "file:../local-lib",
                    "react": "^18.0.0"
                },
                "devDependencies": {
                    "typescript": "^5.0.0",
                    "local-types": "file:../local-types"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&pkg_json).unwrap();

        // Only file: dependencies should be extracted
        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"local-lib"));
        assert!(names.contains(&"local-types"));
    }

    #[test]
    fn test_parse_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"version": "1.0.0"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let result = plugin.parse_project(tmp.path(), &pkg_json);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing or empty"));
    }

    #[test]
    fn test_parse_empty_name() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "", "version": "1.0.0"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let result = plugin.parse_project(tmp.path(), &pkg_json);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing or empty"));
    }

    #[test]
    fn test_parse_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{ invalid json }"#).unwrap();

        let plugin = NodeJsPlugin;
        let result = plugin.parse_project(tmp.path(), &pkg_json);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
    }
}
