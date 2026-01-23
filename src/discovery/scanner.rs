//! Project discovery via WalkBuilder
//!
//! Uses the `ignore` crate to traverse the workspace while respecting
//! .gitignore patterns. Finds projects by their marker files (e.g., package.json)
//! and merges aster.toml overrides.

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{find_aster_toml, parse_aster_toml};
use crate::plugins::{LocalDependency, PluginRegistry, ProjectMetadata, Target, TargetContext};
use crate::targets::TargetResolver;

/// A project discovered during workspace traversal
#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    /// Absolute path to project directory
    pub root: PathBuf,
    /// Path to the native config file (e.g., package.json)
    pub config_path: PathBuf,
    /// Metadata from native config (possibly overridden by aster.toml)
    pub metadata: ProjectMetadata,
    /// Dependencies from native config + aster.toml depends_on
    pub dependencies: Vec<LocalDependency>,
    /// Resolved targets with commands and dependencies
    pub targets: HashMap<String, Target>,
    /// Which plugin discovered this project
    pub plugin_name: String,
    /// Path relative to workspace root (for addressing)
    pub relative_path: PathBuf,
}

/// Discover all projects in the workspace
///
/// Walks the directory tree from workspace_root, respecting .gitignore patterns.
/// For each marker file found (e.g., package.json), the corresponding plugin
/// parses the project. If an aster.toml exists, its overrides are merged.
///
/// Name collisions are resolved by appending the plugin name suffix (e.g., core-nodejs).
pub fn discover_projects(
    workspace_root: &Path,
    registry: &PluginRegistry,
) -> Result<Vec<DiscoveredProject>> {
    // Collect all marker files we're looking for
    let marker_files: Vec<&str> = registry
        .plugins()
        .iter()
        .flat_map(|p| p.marker_files().iter().copied())
        .collect();

    let mut projects: Vec<DiscoveredProject> = Vec::new();

    // Walk the directory tree, respecting gitignore
    let walker = WalkBuilder::new(workspace_root)
        .hidden(false) // Don't skip hidden dirs (except .git itself)
        .git_ignore(true) // Honor .gitignore
        .git_exclude(true) // Honor .git/info/exclude
        .git_global(true) // Honor global gitignore
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // Skip entries we can't read
        };

        // Only process files
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }

        // Check if this is a marker file
        let file_name = match entry.file_name().to_str() {
            Some(name) => name,
            None => continue,
        };

        if !marker_files.contains(&file_name) {
            continue;
        }

        // Find the plugin that handles this marker
        let plugin = match registry.find_by_marker(file_name) {
            Some(p) => p,
            None => continue,
        };

        let config_path = entry.path().to_path_buf();
        let project_dir = match config_path.parent() {
            Some(p) => p,
            None => continue,
        };

        // Parse the project using the plugin
        let mut metadata = plugin
            .parse_project(workspace_root, &config_path)
            .with_context(|| {
                format!(
                    "Failed to parse {} project at {}",
                    plugin.name(),
                    config_path.display()
                )
            })?;

        // Parse dependencies
        let mut dependencies = plugin.parse_dependencies(&config_path).with_context(|| {
            format!(
                "Failed to parse dependencies from {}",
                config_path.display()
            )
        })?;

        // Collect custom targets from aster.toml (if exists)
        let mut custom_targets = HashMap::new();

        // Check for aster.toml and merge overrides
        if let Some(aster_path) = find_aster_toml(project_dir) {
            let aster_config = parse_aster_toml(&aster_path)?;

            // Override name if specified
            if let Some(name) = aster_config.name {
                metadata.name = name;
            }

            // Add depends_on as LocalDependency (with empty path for now, resolved later)
            for dep_addr in aster_config.depends_on {
                dependencies.push(LocalDependency {
                    name: dep_addr.clone(),
                    path: PathBuf::from(dep_addr), // Address string stored as path
                });
            }

            // Collect custom targets for merging with detected
            custom_targets = aster_config.targets;
        }

        // Compute relative path (needed for target resolution)
        let relative_path = project_dir
            .strip_prefix(workspace_root)
            .unwrap_or(project_dir)
            .to_path_buf();

        // Compute project address for //self: resolution
        let project_address = format!("//{}", relative_path.display());

        // Detect available targets from native config
        // Plugin receives full context and handles path resolution internally
        let target_ctx = TargetContext {
            config_path: &config_path,
            project_dir,
            workspace_root,
            dependencies: &dependencies,
        };
        let detected_targets = plugin
            .detect_targets(&target_ctx)
            .with_context(|| format!("Failed to detect targets from {}", config_path.display()))?;

        // Resolve targets: detected + aster.toml overrides, resolving //self: references
        let targets = TargetResolver::resolve(&detected_targets, &custom_targets, &project_address);

        projects.push(DiscoveredProject {
            root: project_dir.to_path_buf(),
            config_path,
            metadata,
            dependencies,
            targets,
            plugin_name: plugin.name().to_string(),
            relative_path,
        });
    }

    // Handle name collisions
    resolve_name_collisions(&mut projects);

    Ok(projects)
}

/// Resolve name collisions by appending plugin suffix
///
/// If two projects have the same name, both get renamed to {name}-{plugin}
fn resolve_name_collisions(projects: &mut [DiscoveredProject]) {
    // Build a map of name -> indices
    let mut name_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, project) in projects.iter().enumerate() {
        name_to_indices
            .entry(project.metadata.name.clone())
            .or_default()
            .push(idx);
    }

    // Rename colliding projects
    for (name, indices) in name_to_indices {
        if indices.len() > 1 {
            // Collision detected - rename all with this name
            for idx in indices {
                let project = &mut projects[idx];
                project.metadata.name = format!("{}-{}", name, project.plugin_name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::NodeJsPlugin;

    fn setup_registry() -> PluginRegistry {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(NodeJsPlugin));
        registry
    }

    #[test]
    fn test_discover_single_project() {
        let tmp = tempfile::tempdir().unwrap();
        let services_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&services_dir).unwrap();

        let pkg_json = services_dir.join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "api", "version": "1.0.0", "scripts": {"test": "jest", "build": "tsc"}}"#,
        )
        .unwrap();

        // Create .git to mark workspace root (for gitignore handling)
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api");
        assert_eq!(projects[0].relative_path, PathBuf::from("services/api"));
        assert_eq!(projects[0].plugin_name, "nodejs");

        // Verify only detected targets are populated (from scripts)
        assert_eq!(
            projects[0].targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            projects[0].targets.get("build").map(|t| &t.command),
            Some(&"npm run build".to_string())
        );
        // deps is always present
        assert_eq!(
            projects[0].targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );
        // lint is not in scripts, so not present
        assert_eq!(projects[0].targets.get("lint"), None);

        // Check that //self: references are resolved to project address
        let test_target = projects[0].targets.get("test").unwrap();
        assert_eq!(
            test_target.depends_on,
            vec!["//services/api:deps".to_string()]
        );
    }

    #[test]
    fn test_discover_project_no_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let services_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&services_dir).unwrap();

        let pkg_json = services_dir.join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "api", "version": "1.0.0"}"#).unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api");
        // Only deps target is defined (no scripts)
        assert_eq!(projects[0].targets.len(), 1);
        assert!(projects[0].targets.contains_key("deps"));
    }

    #[test]
    fn test_discover_multiple_projects() {
        let tmp = tempfile::tempdir().unwrap();

        // Project 1
        let dir1 = tmp.path().join("services/api");
        std::fs::create_dir_all(&dir1).unwrap();
        std::fs::write(
            dir1.join("package.json"),
            r#"{"name": "api", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Project 2
        let dir2 = tmp.path().join("services/web");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("package.json"),
            r#"{"name": "web", "version": "2.0.0"}"#,
        )
        .unwrap();

        // Project 3 - nested
        let dir3 = tmp.path().join("libs/shared");
        std::fs::create_dir_all(&dir3).unwrap();
        std::fs::write(
            dir3.join("package.json"),
            r#"{"name": "shared", "version": "0.1.0"}"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 3);
        let names: Vec<&str> = projects.iter().map(|p| p.metadata.name.as_str()).collect();
        assert!(names.contains(&"api"));
        assert!(names.contains(&"web"));
        assert!(names.contains(&"shared"));
    }

    #[test]
    fn test_discover_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();

        // Regular project
        let dir1 = tmp.path().join("services/api");
        std::fs::create_dir_all(&dir1).unwrap();
        std::fs::write(
            dir1.join("package.json"),
            r#"{"name": "api", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Project in ignored directory
        let dir2 = tmp.path().join("node_modules/some-dep");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("package.json"),
            r#"{"name": "some-dep", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Create .git and .gitignore
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        // Only the api project should be found, not the one in node_modules
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api");
    }

    #[test]
    fn test_discover_merges_aster_toml() {
        let tmp = tempfile::tempdir().unwrap();

        let project_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&project_dir).unwrap();

        // package.json with scripts
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name": "api", "version": "1.0.0", "scripts": {"test": "jest", "build": "tsc", "lint": "eslint ."}}"#,
        )
        .unwrap();

        // aster.toml with name override and custom lint target
        std::fs::write(
            project_dir.join("aster.toml"),
            r#"
name = "api-service"
depends_on = ["//libs/shared:build"]

[targets]
lint = "npm run lint:ci"
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api-service"); // Name overridden

        // Custom lint target from aster.toml overrides detected
        assert_eq!(
            projects[0].targets.get("lint").map(|t| &t.command),
            Some(&"npm run lint:ci".to_string())
        );
        // Detected targets still present
        assert_eq!(
            projects[0].targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            projects[0].targets.get("build").map(|t| &t.command),
            Some(&"npm run build".to_string())
        );

        // depends_on should be in dependencies
        assert!(projects[0]
            .dependencies
            .iter()
            .any(|d| d.name == "//libs/shared:build"));
    }

    #[test]
    fn test_discover_aster_toml_adds_targets() {
        let tmp = tempfile::tempdir().unwrap();

        let project_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&project_dir).unwrap();

        // package.json with only test script
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name": "api", "scripts": {"test": "jest"}}"#,
        )
        .unwrap();

        // aster.toml adds deploy target
        std::fs::write(
            project_dir.join("aster.toml"),
            r#"
[targets]
deploy = "npm run deploy"
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 1);

        // Detected test target present
        assert_eq!(
            projects[0].targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        // Custom deploy target from aster.toml added (with empty depends_on)
        assert_eq!(
            projects[0].targets.get("deploy").map(|t| &t.command),
            Some(&"npm run deploy".to_string())
        );
        assert!(projects[0]
            .targets
            .get("deploy")
            .unwrap()
            .depends_on
            .is_empty());
        // Total: test (detected) + deps (detected) + deploy (custom) = 3
        assert_eq!(projects[0].targets.len(), 3);
    }

    #[test]
    fn test_discover_name_collision() {
        let tmp = tempfile::tempdir().unwrap();

        // Two projects with same name "core"
        let dir1 = tmp.path().join("services/core");
        std::fs::create_dir_all(&dir1).unwrap();
        std::fs::write(
            dir1.join("package.json"),
            r#"{"name": "core", "version": "1.0.0"}"#,
        )
        .unwrap();

        let dir2 = tmp.path().join("libs/core");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("package.json"),
            r#"{"name": "core", "version": "2.0.0"}"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 2);
        // Both should be renamed to core-nodejs
        for project in &projects {
            assert_eq!(project.metadata.name, "core-nodejs");
        }
    }

    #[test]
    fn test_discover_empty_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert!(projects.is_empty());
    }

    #[test]
    fn test_discover_with_file_dependencies() {
        let tmp = tempfile::tempdir().unwrap();

        // Shared lib
        let lib_dir = tmp.path().join("libs/shared");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("package.json"),
            r#"{"name": "shared", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Project depending on shared
        let api_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("package.json"),
            r#"{
                "name": "api",
                "version": "1.0.0",
                "dependencies": {
                    "shared": "file:../../libs/shared"
                }
            }"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 2);

        let api_project = projects.iter().find(|p| p.metadata.name == "api").unwrap();
        assert_eq!(api_project.dependencies.len(), 1);
        assert_eq!(api_project.dependencies[0].name, "shared");
    }

    #[test]
    fn test_discover_target_detection_by_plugin() {
        use crate::plugins::{ElixirPlugin, PythonPlugin};

        let tmp = tempfile::tempdir().unwrap();

        // Node.js project with scripts
        let node_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&node_dir).unwrap();
        std::fs::write(
            node_dir.join("package.json"),
            r#"{"name": "api", "scripts": {"test": "jest", "build": "tsc"}}"#,
        )
        .unwrap();

        // Elixir project (test and build always available, lint only if credo present)
        let elixir_dir = tmp.path().join("services/backend");
        std::fs::create_dir_all(&elixir_dir).unwrap();
        std::fs::write(
            elixir_dir.join("mix.exs"),
            r#"
defmodule Backend.MixProject do
  use Mix.Project

  def project do
    [app: :backend]
  end

  defp deps do
    [{:credo, "~> 1.7"}]
  end
end
"#,
        )
        .unwrap();

        // Python project with pytest and build config
        let python_dir = tmp.path().join("libs/ml");
        std::fs::create_dir_all(&python_dir).unwrap();
        std::fs::write(
            python_dir.join("pyproject.toml"),
            r#"
[project]
name = "ml"
version = "1.0.0"

[build-system]
requires = ["setuptools"]

[tool.pytest.ini_options]
testpaths = ["tests"]

[tool.ruff]
line-length = 100
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        // Register all plugins
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(NodeJsPlugin));
        registry.register(Box::new(ElixirPlugin));
        registry.register(Box::new(PythonPlugin));

        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 3);

        // Verify Node.js targets (detected from scripts)
        let node_project = projects.iter().find(|p| p.plugin_name == "nodejs").unwrap();
        assert_eq!(
            node_project.targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            node_project.targets.get("build").map(|t| &t.command),
            Some(&"npm run build".to_string())
        );
        assert_eq!(
            node_project.targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );
        // lint not in scripts, so not present
        assert_eq!(node_project.targets.get("lint"), None);

        // Verify Elixir targets (test/build/deps always, lint if credo present)
        let elixir_project = projects.iter().find(|p| p.plugin_name == "elixir").unwrap();
        assert_eq!(
            elixir_project.targets.get("test").map(|t| &t.command),
            Some(&"mix test".to_string())
        );
        assert_eq!(
            elixir_project.targets.get("build").map(|t| &t.command),
            Some(&"mix compile".to_string())
        );
        assert_eq!(
            elixir_project.targets.get("deps").map(|t| &t.command),
            Some(&"mix deps.get".to_string())
        );
        assert_eq!(
            elixir_project.targets.get("lint").map(|t| &t.command),
            Some(&"mix credo".to_string())
        );

        // Verify Python targets (detected from config sections)
        let python_project = projects.iter().find(|p| p.plugin_name == "python").unwrap();
        assert_eq!(
            python_project.targets.get("test").map(|t| &t.command),
            Some(&"pytest".to_string())
        );
        assert_eq!(
            python_project.targets.get("build").map(|t| &t.command),
            Some(&"python -m build".to_string())
        );
        assert_eq!(
            python_project.targets.get("deps").map(|t| &t.command),
            Some(&"pip install -e .".to_string())
        );
        assert_eq!(
            python_project.targets.get("lint").map(|t| &t.command),
            Some(&"ruff check .".to_string())
        );
    }

    #[test]
    fn test_discover_elixir_no_credo() {
        use crate::plugins::ElixirPlugin;

        let tmp = tempfile::tempdir().unwrap();

        // Elixir project without credo
        let elixir_dir = tmp.path().join("services/backend");
        std::fs::create_dir_all(&elixir_dir).unwrap();
        std::fs::write(
            elixir_dir.join("mix.exs"),
            r#"
defmodule Backend.MixProject do
  use Mix.Project

  def project do
    [app: :backend]
  end

  defp deps do
    [{:jason, "~> 1.4"}]
  end
end
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(ElixirPlugin));

        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 1);
        let project = &projects[0];

        // test, build, and deps always available
        assert_eq!(
            project.targets.get("test").map(|t| &t.command),
            Some(&"mix test".to_string())
        );
        assert_eq!(
            project.targets.get("build").map(|t| &t.command),
            Some(&"mix compile".to_string())
        );
        assert_eq!(
            project.targets.get("deps").map(|t| &t.command),
            Some(&"mix deps.get".to_string())
        );
        // lint NOT available (no credo)
        assert_eq!(project.targets.get("lint"), None);
    }
}
