//! Python language plugin for pyproject.toml parsing

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{LanguagePlugin, LocalDependency, ProjectMetadata};

/// Regex to extract path from PEP 621 format: `pkg @ file:../path`
static PEP621_FILE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S+)\s*@\s*file:(.+)$").expect("Invalid PEP621_FILE_REGEX")
});

/// Root structure for pyproject.toml
#[derive(Deserialize, Default)]
struct PyProjectToml {
    project: Option<PepProject>,
    tool: Option<ToolSection>,
}

/// PEP 621 project metadata section
#[derive(Deserialize, Default)]
struct PepProject {
    name: Option<String>,
    dependencies: Option<Vec<String>>,
}

/// Tool section containing tool-specific configurations
#[derive(Deserialize, Default)]
struct ToolSection {
    poetry: Option<PoetrySection>,
}

/// Poetry-specific configuration
#[derive(Deserialize, Default)]
struct PoetrySection {
    name: Option<String>,
    dependencies: Option<HashMap<String, PoetryDep>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<HashMap<String, PoetryDep>>,
}

/// Poetry dependency can be either a simple version string or a complex table
#[derive(Deserialize)]
#[serde(untagged)]
enum PoetryDep {
    /// Simple version string: `"^1.0"`
    #[allow(dead_code)]
    Version(String),
    /// Complex table: `{path = "../lib", develop = true}`
    Table(PoetryDepTable),
}

/// Poetry dependency table with optional fields
#[derive(Deserialize, Default)]
struct PoetryDepTable {
    path: Option<String>,
    #[allow(dead_code)]
    develop: Option<bool>,
    #[allow(dead_code)]
    version: Option<String>,
    #[allow(dead_code)]
    git: Option<String>,
}

/// Python plugin for discovering and parsing pyproject.toml projects
pub struct PythonPlugin;

impl LanguagePlugin for PythonPlugin {
    fn name(&self) -> &str {
        "python"
    }

    fn marker_files(&self) -> &[&str] {
        &["pyproject.toml"]
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pyproject: PyProjectToml = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        // Priority: project.name (PEP 621) > tool.poetry.name
        let name = pyproject
            .project
            .as_ref()
            .and_then(|p| p.name.clone())
            .or_else(|| {
                pyproject
                    .tool
                    .as_ref()
                    .and_then(|t| t.poetry.as_ref())
                    .and_then(|p| p.name.clone())
            })
            .ok_or_else(|| {
                anyhow!(
                    "Could not find project name in {}. Expected [project].name or [tool.poetry].name",
                    config_path.display()
                )
            })?;

        Ok(ProjectMetadata {
            name,
            version: None,
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pyproject: PyProjectToml = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let mut deps = Vec::new();

        // Extract PEP 621 path dependencies: `pkg @ file:../path`
        if let Some(project) = &pyproject.project {
            if let Some(dependencies) = &project.dependencies {
                for dep_str in dependencies {
                    if let Some((name, path)) = extract_pep621_file_path(dep_str) {
                        deps.push(LocalDependency {
                            name: name.to_string(),
                            path: PathBuf::from(path),
                        });
                    }
                }
            }
        }

        // Extract Poetry path dependencies
        if let Some(tool) = &pyproject.tool {
            if let Some(poetry) = &tool.poetry {
                // Check both dependencies and dev-dependencies
                for dep_map in [&poetry.dependencies, &poetry.dev_dependencies]
                    .into_iter()
                    .flatten()
                {
                    for (name, dep) in dep_map {
                        if let PoetryDep::Table(table) = dep {
                            if let Some(path) = &table.path {
                                deps.push(LocalDependency {
                                    name: name.clone(),
                                    path: PathBuf::from(path),
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(deps)
    }
}

/// Extract name and path from PEP 621 file dependency format: `pkg @ file:../path`
fn extract_pep621_file_path(dep: &str) -> Option<(&str, &str)> {
    PEP621_FILE_REGEX.captures(dep).map(|caps| {
        let name = caps.get(1).unwrap().as_str().trim();
        let path = caps.get(2).unwrap().as_str().trim();
        // Handle file:///absolute/path vs file:relative/path
        let path = path.strip_prefix("//").unwrap_or(path);
        (name, path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pep621_project() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-python-app"
version = "1.0.0"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let metadata = plugin.parse_project(tmp.path(), &pyproject).unwrap();

        assert_eq!(metadata.name, "my-python-app");
    }

    #[test]
    fn test_parse_poetry_project() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[tool.poetry]
name = "my-poetry-app"
version = "1.0.0"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let metadata = plugin.parse_project(tmp.path(), &pyproject).unwrap();

        assert_eq!(metadata.name, "my-poetry-app");
    }

    #[test]
    fn test_parse_poetry_path_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[tool.poetry]
name = "my-app"

[tool.poetry.dependencies]
python = "^3.11"
my-lib = {path = "../my-lib"}
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let deps = plugin.parse_dependencies(&pyproject).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "my-lib");
        assert_eq!(deps[0].path, PathBuf::from("../my-lib"));
    }

    #[test]
    fn test_parse_poetry_path_with_develop() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[tool.poetry]
name = "my-app"

[tool.poetry.dependencies]
python = "^3.11"
shared = {path = "../shared", develop = true}
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let deps = plugin.parse_dependencies(&pyproject).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "shared");
        assert_eq!(deps[0].path, PathBuf::from("../shared"));
    }

    #[test]
    fn test_parse_poetry_dev_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[tool.poetry]
name = "my-app"

[tool.poetry.dependencies]
python = "^3.11"

[tool.poetry.dev-dependencies]
test-utils = {path = "../test-utils"}
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let deps = plugin.parse_dependencies(&pyproject).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "test-utils");
        assert_eq!(deps[0].path, PathBuf::from("../test-utils"));
    }

    #[test]
    fn test_parse_pep621_file_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"
dependencies = [
    "requests>=2.0",
    "my-lib @ file:../my-lib",
]
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let deps = plugin.parse_dependencies(&pyproject).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "my-lib");
        assert_eq!(deps[0].path, PathBuf::from("../my-lib"));
    }

    #[test]
    fn test_priority_pep621_over_poetry() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "pep621-name"

[tool.poetry]
name = "poetry-name"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let metadata = plugin.parse_project(tmp.path(), &pyproject).unwrap();

        // PEP 621 project.name takes priority over tool.poetry.name
        assert_eq!(metadata.name, "pep621-name");
    }

    #[test]
    fn test_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
version = "1.0.0"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let result = plugin.parse_project(tmp.path(), &pyproject);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Could not find project name"));
    }

    #[test]
    fn test_no_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"
dependencies = [
    "requests>=2.0",
    "flask>=2.0",
]

[tool.poetry.dependencies]
python = "^3.11"
django = "^4.0"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let deps = plugin.parse_dependencies(&pyproject).unwrap();

        assert!(deps.is_empty());
    }

    #[test]
    fn test_mixed_poetry_and_pep621_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"
dependencies = [
    "pep621-lib @ file:../pep621-lib",
]

[tool.poetry]
name = "my-app"

[tool.poetry.dependencies]
python = "^3.11"
poetry-lib = {path = "../poetry-lib"}
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let deps = plugin.parse_dependencies(&pyproject).unwrap();

        // Should extract from both PEP 621 and Poetry sections
        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"pep621-lib"));
        assert!(names.contains(&"poetry-lib"));
    }
}
