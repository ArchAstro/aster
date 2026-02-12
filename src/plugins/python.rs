//! Python language plugin for pyproject.toml parsing

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{
    LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetCapability, TargetContext,
};

/// Regex to extract path from PEP 621 format: `pkg @ file:../path`
static PEP621_FILE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\S+)\s*@\s*file:(.+)$").expect("Invalid PEP621_FILE_REGEX"));

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

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let content = std::fs::read_to_string(ctx.config_path)
            .with_context(|| format!("Failed to read {}", ctx.config_path.display()))?;

        let mut targets = HashMap::new();

        // Detect package manager and set up command wrapper
        // Poetry projects use `poetry run` to execute commands in the venv
        // uv projects use `uv run` to execute commands in the venv
        let is_poetry = content.contains("[tool.poetry]");
        let has_uv_lock = ctx.project_dir.join("uv.lock").exists();

        let (deps_command, cmd_prefix) = if is_poetry {
            ("poetry install", "poetry run ")
        } else if has_uv_lock {
            ("uv sync", "uv run ")
        } else {
            ("pip install -e .", "")
        };
        let wrap_cmd = |cmd: &str| format!("{cmd_prefix}{cmd}");

        targets.insert(
            "deps".to_string(),
            Target {
                command: deps_command.to_string(),
                depends_on: vec![],
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec!["pip_cache".into()],
            },
        );

        // Resolve dependency paths to project addresses
        let dependency_addresses = resolve_dependency_addresses(ctx);

        // Build dependencies for non-deps targets:
        // - //self:deps (install our own dependencies first)
        // - :build for each project dependency (they must be built first)
        let mut base_deps = vec!["//self:deps".to_string()];
        for dep_addr in &dependency_addresses {
            base_deps.push(format!("{dep_addr}:build"));
        }

        // Check for pytest configuration
        if content.contains("[tool.pytest")
            || content.contains("pytest")
            || content.contains("[project.optional-dependencies]")
        {
            let mut test_caps = HashSet::new();
            test_caps.insert(TargetCapability::FilesList);
            test_caps.insert(TargetCapability::WarningsAsErrors);
            targets.insert(
                "test".to_string(),
                Target {
                    command: wrap_cmd("pytest"),
                    depends_on: base_deps.clone(),
                    capabilities: test_caps,
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                    exclusive_resources: vec![],
                },
            );
        }

        // Check for ruff
        if content.contains("[tool.ruff]") || content.contains("ruff") {
            targets.insert(
                "lint".to_string(),
                Target {
                    command: wrap_cmd("ruff check ."),
                    depends_on: base_deps,
                    capabilities: HashSet::new(),
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                    exclusive_resources: vec![],
                },
            );
        }

        // format target: prefer ruff, fall back to black
        let format_deps = vec!["//self:deps".to_string()];
        if content.contains("[tool.ruff]") {
            targets.insert(
                "format".to_string(),
                Target {
                    command: wrap_cmd("ruff format ."),
                    depends_on: format_deps,
                    capabilities: HashSet::new(),
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                    exclusive_resources: vec![],
                },
            );
        } else if content.contains("[tool.black]") {
            targets.insert(
                "format".to_string(),
                Target {
                    command: wrap_cmd("black ."),
                    depends_on: format_deps,
                    capabilities: HashSet::new(),
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                    exclusive_resources: vec![],
                },
            );
        }

        // Add clean target
        if let Some(clean) = self.clean_target(ctx) {
            targets.insert("clean".to_string(), clean);
        }

        Ok(targets)
    }

    fn with_files_list(
        &self,
        target_name: &str,
        command: &str,
        files: &[PathBuf],
    ) -> Option<String> {
        // Only test target supports file list
        if target_name != "test" {
            return None;
        }

        // Filter to Python test files only
        let test_files: Vec<&PathBuf> = files
            .iter()
            .filter(|f| {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let path_str = f.to_string_lossy();
                name.starts_with("test_")
                    || name.ends_with("_test.py")
                    || path_str.contains("tests/")
                    || path_str.contains("test/")
            })
            .collect();

        if test_files.is_empty() {
            // No test files in the change set - run full test suite
            return None;
        }

        // pytest file1.py file2.py (pytest accepts file paths directly)
        let file_args: Vec<String> = test_files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect();

        Some(format!("{} {}", command, file_args.join(" ")))
    }

    fn with_warnings_as_errors(&self, target_name: &str, command: &str) -> Option<String> {
        // pytest supports -W error to treat warnings as errors
        if target_name == "test" {
            Some(format!("{command} -W error"))
        } else {
            None
        }
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        // deps target only cares about dependency specification files,
        // not source code — source changes shouldn't re-install dependencies
        if target_name == "deps" {
            return super::CacheInputs {
                source_globs: vec![],
                config_files: vec![
                    "pyproject.toml".to_string(),
                    "poetry.lock".to_string(),
                    "uv.lock".to_string(),
                    "requirements.txt".to_string(),
                    "setup.py".to_string(),
                    "setup.cfg".to_string(),
                    // Include venv config so cache invalidates when the venv is
                    // deleted, recreated, or rebuilt with a different Python version.
                    ".venv/pyvenv.cfg".to_string(),
                ],
                env_vars: vec!["PYTHONPATH".to_string()],
            };
        }

        let mut inputs = super::CacheInputs {
            source_globs: vec!["src/**/*.py".to_string(), "**/*.py".to_string()],
            config_files: vec![
                "pyproject.toml".to_string(),
                "poetry.lock".to_string(),
                "uv.lock".to_string(),
                "requirements.txt".to_string(),
                "setup.py".to_string(),
                "setup.cfg".to_string(),
            ],
            env_vars: vec!["PYTHONPATH".to_string(), "CI".to_string()],
        };

        if target_name == "test" {
            inputs.source_globs.push("tests/**/*.py".to_string());
            inputs.source_globs.push("*_test.py".to_string());
            inputs.source_globs.push("test_*.py".to_string());
        }

        inputs
    }

    fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
        let mut dirs_to_clean = vec![
            "__pycache__",
            ".pytest_cache",
            "*.egg-info",
            "dist",
            "build",
        ];

        // Add .venv if it exists
        if ctx.project_dir.join(".venv").exists() {
            dirs_to_clean.insert(0, ".venv");
        }

        let command = format!("rm -rf {}", dirs_to_clean.join(" "));

        Some(Target {
            command,
            depends_on: vec![],
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: true,
            working_dir: None,
            exclusive_resources: vec![],
        })
    }
}

/// Resolve LocalDependency paths to project addresses
fn resolve_dependency_addresses(ctx: &TargetContext) -> Vec<String> {
    ctx.dependencies
        .iter()
        .filter_map(|dep| {
            let path_str = dep.path.to_string_lossy();
            if path_str.starts_with("//") {
                // Already an address - strip any target suffix
                let addr = path_str.split(':').next().unwrap_or(&path_str);
                Some(addr.to_string())
            } else {
                // Resolve relative path to address
                let resolved = ctx.project_dir.join(&dep.path);
                let normalized = resolved.canonicalize().ok()?;
                let dep_relative = normalized.strip_prefix(ctx.workspace_root).ok()?;
                Some(format!("//{}", dep_relative.display()))
            }
        })
        .collect()
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

    fn make_context<'a>(
        config_path: &'a Path,
        workspace_root: &'a Path,
        dependencies: &'a [LocalDependency],
    ) -> TargetContext<'a> {
        TargetContext {
            config_path,
            project_dir: config_path.parent().unwrap(),
            workspace_root,
            dependencies,
        }
    }

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

    #[test]
    fn test_detect_targets_with_all_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.pytest.ini_options]
testpaths = ["tests"]

[tool.ruff]
line-length = 100
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"pytest".to_string())
        );
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"ruff check .".to_string())
        );
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"pip install -e .".to_string())
        );
        // No build target for Python (use aster.toml if needed)
        assert_eq!(targets.get("build"), None);

        // Check dependencies
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//self:deps"]);
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
    }

    #[test]
    fn test_detect_targets_only_pytest() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.pytest.ini_options]
testpaths = ["tests"]
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"pytest".to_string())
        );
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"pip install -e .".to_string())
        );
        assert_eq!(targets.get("build"), None);
        assert_eq!(targets.get("lint"), None);
    }

    #[test]
    fn test_detect_targets_no_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"
version = "1.0.0"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // deps and clean are always present
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"pip install -e .".to_string())
        );
        assert!(targets.contains_key("clean"));
    }

    #[test]
    fn test_detect_targets_poetry_project() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[tool.poetry]
name = "my-app"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.11"

[tool.pytest.ini_options]
testpaths = ["tests"]

[tool.ruff]
line-length = 100
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Poetry projects use "poetry install" for deps
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"poetry install".to_string())
        );

        // Poetry projects wrap tool commands with "poetry run"
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"poetry run pytest".to_string())
        );
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"poetry run ruff check .".to_string())
        );
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"poetry run ruff format .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_uv_project() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"
version = "1.0.0"

[tool.pytest.ini_options]
testpaths = ["tests"]

[tool.ruff]
line-length = 100
"#,
        )
        .unwrap();

        // Create uv.lock to indicate uv project
        std::fs::write(tmp.path().join("uv.lock"), "").unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // uv projects use "uv sync" for deps
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"uv sync".to_string())
        );

        // uv projects wrap tool commands with "uv run"
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"uv run pytest".to_string())
        );
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"uv run ruff check .".to_string())
        );
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"uv run ruff format .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_format_with_ruff() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.ruff]
line-length = 100
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses ruff
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"ruff format .".to_string())
        );

        // Check dependencies (only deps, no build for Python)
        let format_deps = &targets.get("format").unwrap().depends_on;
        assert_eq!(format_deps, &vec!["//self:deps".to_string()]);
    }

    #[test]
    fn test_detect_targets_format_with_black() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.black]
line-length = 100
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses black (no ruff)
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"black .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_format_prefers_ruff_over_black() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.ruff]
line-length = 100

[tool.black]
line-length = 100
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format prefers ruff over black
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"ruff format .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_no_format_without_formatter() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"
version = "1.0.0"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // no format target (no ruff or black)
        assert_eq!(targets.get("format"), None);
    }

    #[test]
    fn test_with_files_list_filters_test_files() {
        let plugin = PythonPlugin;
        let files = vec![
            PathBuf::from("src/main.py"),
            PathBuf::from("tests/test_main.py"),
            PathBuf::from("tests/test_utils.py"),
            PathBuf::from("src/utils_test.py"),
            PathBuf::from("pyproject.toml"),
        ];

        let result = plugin.with_files_list("test", "pytest", &files);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.starts_with("pytest "));
        assert!(cmd.contains("test_main.py"));
        assert!(cmd.contains("test_utils.py"));
        assert!(cmd.contains("utils_test.py"));
        assert!(!cmd.contains("src/main.py"));
        assert!(!cmd.contains("pyproject.toml"));
    }

    #[test]
    fn test_with_files_list_returns_none_for_non_test_target() {
        let plugin = PythonPlugin;
        let files = vec![PathBuf::from("tests/test_main.py")];

        let result = plugin.with_files_list("build", "python -m build", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_returns_none_when_no_test_files() {
        let plugin = PythonPlugin;
        let files = vec![
            PathBuf::from("src/main.py"),
            PathBuf::from("src/utils.py"),
            PathBuf::from("pyproject.toml"),
        ];

        let result = plugin.with_files_list("test", "pytest", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_test_target_has_files_list_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.pytest.ini_options]
testpaths = ["tests"]
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));

        let deps_target = targets.get("deps").unwrap();
        assert!(!deps_target
            .capabilities
            .contains(&TargetCapability::FilesList));
    }

    #[test]
    fn test_test_target_has_warnings_as_errors_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "my-app"

[tool.pytest.ini_options]
testpaths = ["tests"]
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::WarningsAsErrors));

        let deps_target = targets.get("deps").unwrap();
        assert!(!deps_target
            .capabilities
            .contains(&TargetCapability::WarningsAsErrors));
    }

    #[test]
    fn test_with_warnings_as_errors_for_test() {
        let plugin = PythonPlugin;
        let result = plugin.with_warnings_as_errors("test", "pytest");
        assert_eq!(result, Some("pytest -W error".to_string()));
    }

    #[test]
    fn test_with_warnings_as_errors_returns_none_for_unsupported_target() {
        let plugin = PythonPlugin;
        let result = plugin.with_warnings_as_errors("deps", "pip install -e .");
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_targets_has_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "mypackage"
"#,
        )
        .unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert!(clean.command.contains("__pycache__"));
        assert!(clean.command.contains(".pytest_cache"));
        assert!(clean.invalidates_cache);
    }

    #[test]
    fn test_clean_target_detects_venv() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            r#"
[project]
name = "mypackage"
"#,
        )
        .unwrap();

        // Create .venv directory
        std::fs::create_dir(tmp.path().join(".venv")).unwrap();

        let plugin = PythonPlugin;
        let ctx = make_context(&pyproject, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert!(clean.command.contains(".venv"));
    }

    #[test]
    fn test_deps_cache_inputs_include_venv_config() {
        let plugin = PythonPlugin;
        let inputs = plugin.cache_inputs("deps");

        // deps cache should include .venv/pyvenv.cfg so that deleting or
        // recreating the venv invalidates the cache
        assert!(
            inputs.config_files.contains(&".venv/pyvenv.cfg".to_string()),
            "deps cache_inputs should include .venv/pyvenv.cfg"
        );

        // deps should NOT include source globs (source changes don't require reinstall)
        assert!(inputs.source_globs.is_empty());
    }

    #[test]
    fn test_non_deps_cache_inputs_exclude_venv_config() {
        let plugin = PythonPlugin;

        // test/lint/format targets should NOT include venv config —
        // they rely on the deps target dependency chain for invalidation
        for target in &["test", "lint", "format"] {
            let inputs = plugin.cache_inputs(target);
            assert!(
                !inputs.config_files.contains(&".venv/pyvenv.cfg".to_string()),
                "{target} cache_inputs should not include .venv/pyvenv.cfg"
            );
        }
    }
}
