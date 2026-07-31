//! Project discovery via WalkBuilder
//!
//! Uses the `ignore` crate to traverse the workspace while respecting
//! .gitignore patterns. Finds projects by their marker files (e.g., package.json)
//! and merges aster.toml overrides.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{find_aster_toml, parse_aster_toml, WorkspaceConfig};
use crate::plugins::{LocalDependency, PluginRegistry, ProjectMetadata, Target, TargetContext};
use crate::targets::TargetResolver;

/// Build a GlobSet from ignore patterns
fn build_ignore_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).with_context(|| format!("Invalid ignore pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .context("Failed to build ignore pattern set")
}

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

/// Intermediate struct for first-pass discovery (before target detection)
struct PartialProject {
    root: PathBuf,
    config_path: PathBuf,
    metadata: ProjectMetadata,
    dependencies: Vec<LocalDependency>,
    custom_targets: HashMap<String, crate::config::TargetConfig>,
    explicit_target_dependencies: Vec<(String, String)>,
    plugin_name: String,
    relative_path: PathBuf,
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
    // Load workspace config for ignore patterns
    let workspace_config = WorkspaceConfig::load(workspace_root)?;
    let ignore_set = build_ignore_set(&workspace_config.ignore)?;

    let mut partial_projects: Vec<PartialProject> = Vec::new();

    // Phase 1: Walk the directory tree and collect project metadata + dependencies
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

        // Check if this path should be ignored
        let relative_path = project_dir
            .strip_prefix(workspace_root)
            .unwrap_or(project_dir);
        if ignore_set.is_match(relative_path) {
            continue;
        }

        // Check if the plugin wants to skip this config file
        // (e.g., Elixir umbrella roots aren't buildable projects)
        if plugin.should_skip(&config_path) {
            continue;
        }

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
        let mut explicit_target_dependencies = Vec::new();

        // Check for aster.toml and merge overrides
        if let Some(aster_path) = find_aster_toml(project_dir) {
            let aster_config = parse_aster_toml(&aster_path)?;

            // Override name if specified
            if let Some(name) = aster_config.name {
                metadata.name = name;
            }

            // Add depends_on as LocalDependency (with empty path for now, resolved later)
            for dep_addr in aster_config.depends_on {
                let dependency_project = dep_addr.split(':').next().unwrap_or(&dep_addr);
                explicit_target_dependencies.push((
                    format!("project //{}", relative_path.display()),
                    format!("{dependency_project}:build"),
                ));
                dependencies.push(LocalDependency {
                    name: dep_addr.clone(),
                    path: PathBuf::from(dep_addr), // Address string stored as path
                });
            }

            // Collect custom targets for merging with detected
            custom_targets = aster_config.targets;
            let project_address = format!("//{}", relative_path.display());
            for (target_name, target) in &custom_targets {
                if let crate::config::TargetConfig::Alias(alias) = target {
                    if alias.alias.starts_with("//") && !alias.alias.starts_with("//self:") {
                        anyhow::bail!(
                            "target {project_address}:{target_name} has external alias source {}; \
                             aliases must use a local target name or //self:<target>",
                            alias.alias
                        );
                    }
                    let alias_dependency = alias
                        .alias
                        .strip_prefix("//self:")
                        .map(|name| format!("{project_address}:{name}"))
                        .unwrap_or_else(|| format!("{project_address}:{}", alias.alias));
                    explicit_target_dependencies.push((
                        format!("target {project_address}:{target_name}"),
                        alias_dependency,
                    ));
                }
                for dependency in target.depends_on() {
                    let dependency = dependency
                        .strip_prefix("//self:")
                        .map(|name| format!("{project_address}:{name}"))
                        .unwrap_or_else(|| dependency.clone());
                    explicit_target_dependencies.push((
                        format!("target {project_address}:{target_name}"),
                        dependency,
                    ));
                }
            }
        }

        // Compute relative path (needed for target resolution)
        let relative_path = project_dir
            .strip_prefix(workspace_root)
            .unwrap_or(project_dir)
            .to_path_buf();

        partial_projects.push(PartialProject {
            root: project_dir.to_path_buf(),
            config_path,
            metadata,
            dependencies,
            custom_targets,
            explicit_target_dependencies,
            plugin_name: plugin.name().to_string(),
            relative_path,
        });
    }

    // Phase 2: Resolve special dependency references
    // This must happen BEFORE target detection so dependencies are resolved correctly
    resolve_umbrella_dependencies_partial(&mut partial_projects);
    resolve_uv_workspace_dependencies_partial(&mut partial_projects);
    resolve_ruby_workspace_dependencies_partial(&mut partial_projects);

    // Phase 3: Detect targets with resolved dependencies
    let mut projects: Vec<DiscoveredProject> = Vec::new();
    let mut explicit_target_dependencies = Vec::new();
    for partial in partial_projects {
        let plugin = registry
            .find_by_name(&partial.plugin_name)
            .expect("Plugin should exist");

        // Compute project address for //self: resolution
        let project_address = format!("//{}", partial.relative_path.display());

        // Detect available targets from native config
        let target_ctx = TargetContext {
            config_path: &partial.config_path,
            project_dir: &partial.root,
            workspace_root,
            dependencies: &partial.dependencies,
        };
        let detected_targets = plugin.detect_targets(&target_ctx).with_context(|| {
            format!(
                "Failed to detect targets from {}",
                partial.config_path.display()
            )
        })?;

        // Resolve targets: detected + aster.toml overrides, resolving //self: references
        let targets =
            TargetResolver::resolve(&detected_targets, &partial.custom_targets, &project_address);
        explicit_target_dependencies.extend(partial.explicit_target_dependencies);

        projects.push(DiscoveredProject {
            root: partial.root,
            config_path: partial.config_path,
            metadata: partial.metadata,
            dependencies: partial.dependencies,
            targets,
            plugin_name: partial.plugin_name,
            relative_path: partial.relative_path,
        });
    }

    // Phase 4: Handle name collisions
    resolve_name_collisions(&mut projects);

    validate_project_addresses(&projects)?;
    normalize_target_dependencies(&mut projects, &explicit_target_dependencies)?;

    Ok(projects)
}

fn validate_project_addresses(projects: &[DiscoveredProject]) -> Result<()> {
    let mut seen: HashMap<String, &DiscoveredProject> = HashMap::new();

    for project in projects {
        let address = format!("//{}", project.relative_path.display());
        if let Some(existing) = seen.insert(address.clone(), project) {
            anyhow::bail!(
                "Multiple projects resolve to {address}: {} ({}) and {} ({}). \
                 Put each language project in its own directory.",
                existing.config_path.display(),
                existing.plugin_name,
                project.config_path.display(),
                project.plugin_name,
            );
        }
    }

    Ok(())
}

fn normalize_target_dependencies(
    projects: &mut [DiscoveredProject],
    explicit_dependencies: &[(String, String)],
) -> Result<()> {
    let available: std::collections::HashSet<String> = projects
        .iter()
        .flat_map(|project| {
            let project_address = format!("//{}", project.relative_path.display());
            project
                .targets
                .keys()
                .map(move |target| format!("{project_address}:{target}"))
        })
        .collect();

    for (source, dependency) in explicit_dependencies {
        if !available.contains(dependency) {
            anyhow::bail!("{source} depends on missing target {dependency}");
        }
    }

    // Language plugins infer `:build` edges from native path/workspace
    // dependencies. Not every dependency project defines a build target, so
    // discard only those unavailable inferred edges after explicit config has
    // been validated.
    for project in projects {
        for target in project.targets.values_mut() {
            target
                .depends_on
                .retain(|dependency| available.contains(dependency));
        }
    }

    Ok(())
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

/// Resolve in_umbrella dependencies for Elixir projects (partial discovery phase)
///
/// In Elixir umbrella projects, `{:dep_name, in_umbrella: true}` dependencies
/// use app names that may differ from directory names. This function resolves
/// these by looking up the app name in discovered projects.
fn resolve_umbrella_dependencies_partial(projects: &mut [PartialProject]) {
    // Build a map of Elixir app name -> project address
    let app_to_address: HashMap<String, String> = projects
        .iter()
        .filter(|p| p.plugin_name == "elixir")
        .map(|p| {
            (
                p.metadata.name.clone(),
                format!("//{}", p.relative_path.display()),
            )
        })
        .collect();

    // Resolve in_umbrella: dependencies
    for project in projects.iter_mut() {
        if project.plugin_name != "elixir" {
            continue;
        }

        for dep in &mut project.dependencies {
            let path_str = dep.path.to_string_lossy();
            if let Some(app_name) = path_str.strip_prefix("in_umbrella:") {
                // Look up the app name in discovered projects
                if let Some(address) = app_to_address.get(app_name) {
                    dep.path = PathBuf::from(address.clone());
                }
                // If not found, keep the original path (will fail to resolve later)
            }
        }
    }
}

/// Resolve uv workspace dependencies for Python projects
///
/// In uv workspaces, `[tool.uv.sources]` can reference other workspace members
/// via `{ workspace = true }`. The Python plugin emits these as sentinel paths
/// `uv_workspace:<name>`. This function resolves them to project addresses by
/// matching the dependency name against discovered Python project names.
fn resolve_uv_workspace_dependencies_partial(projects: &mut [PartialProject]) {
    // Build a map of Python project name -> project address
    let name_to_address: HashMap<String, String> = projects
        .iter()
        .filter(|p| p.plugin_name == "python")
        .map(|p| {
            (
                p.metadata.name.clone(),
                format!("//{}", p.relative_path.display()),
            )
        })
        .collect();

    // Resolve uv_workspace: dependencies
    for project in projects.iter_mut() {
        if project.plugin_name != "python" {
            continue;
        }

        for dep in &mut project.dependencies {
            let path_str = dep.path.to_string_lossy();
            if let Some(dep_name) = path_str.strip_prefix("uv_workspace:") {
                if let Some(address) = name_to_address.get(dep_name) {
                    dep.path = PathBuf::from(address.clone());
                }
            }
        }
    }
}

/// Resolve gemspec runtime dependencies to sibling gems in the same workspace.
/// Unmatched names are ordinary remote gems and are removed: LocalDependency
/// represents only relationships Aster can resolve within this checkout.
fn resolve_ruby_workspace_dependencies_partial(projects: &mut [PartialProject]) {
    let name_to_address: HashMap<String, String> = projects
        .iter()
        .filter(|project| project.plugin_name == "ruby")
        .map(|project| {
            (
                project.metadata.name.clone(),
                format!("//{}", project.relative_path.display()),
            )
        })
        .collect();

    for project in projects
        .iter_mut()
        .filter(|project| project.plugin_name == "ruby")
    {
        project.dependencies.retain_mut(|dependency| {
            let path = dependency.path.to_string_lossy();
            let Some(name) = path.strip_prefix("ruby_workspace:") else {
                return true;
            };
            if let Some(address) = name_to_address.get(name) {
                dependency.path = PathBuf::from(address);
                true
            } else {
                false
            }
        });
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
        // deps and clean targets are defined (no scripts)
        assert_eq!(projects[0].targets.len(), 2);
        assert!(projects[0].targets.contains_key("deps"));
        assert!(projects[0].targets.contains_key("clean"));
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
    fn discovers_gradle_kotlin_dsl_modules_without_flattening_native_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join("settings.gradle.kts"),
            r#"rootProject.name = "sample"
include("shared", "app")
"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("gradlew"), "").unwrap();

        for module in ["shared", "app"] {
            let directory = tmp.path().join(module);
            std::fs::create_dir_all(directory.join("src/main/java")).unwrap();
            std::fs::write(
                directory.join(format!("{module}.gradle.kts")),
                if module == "app" {
                    r#"plugins { java }
dependencies { implementation(project(":shared")) }
"#
                } else {
                    "plugins { java }\n"
                },
            )
            .unwrap();
        }

        let projects = discover_projects(tmp.path(), &PluginRegistry::with_all_plugins()).unwrap();
        assert_eq!(projects.len(), 2);
        let app = projects
            .iter()
            .find(|project| project.relative_path == Path::new("app"))
            .unwrap();
        assert_eq!(app.plugin_name, "gradle");
        assert_eq!(app.targets["build"].command, "./gradlew :app:build");
        assert!(app.dependencies.is_empty());
        assert_eq!(app.targets["build"].depends_on, vec!["//app:deps"]);
    }

    #[test]
    fn discovers_maven_reactor_modules_without_flattening_native_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join("pom.xml"),
            r#"<project>
  <artifactId>sample-parent</artifactId>
  <packaging>pom</packaging>
  <modules><module>shared</module><module>app</module></modules>
</project>"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("mvnw"), "").unwrap();

        for module in ["shared", "app"] {
            let directory = tmp.path().join(module);
            std::fs::create_dir_all(directory.join("src/main/java")).unwrap();
            let dependencies = if module == "app" {
                "<dependencies><dependency><groupId>dev.example</groupId><artifactId>shared</artifactId></dependency></dependencies>"
            } else {
                ""
            };
            std::fs::write(
                directory.join("pom.xml"),
                format!("<project><artifactId>{module}</artifactId>{dependencies}</project>"),
            )
            .unwrap();
        }

        let projects = discover_projects(tmp.path(), &PluginRegistry::with_all_plugins()).unwrap();
        assert_eq!(projects.len(), 2);
        let app = projects
            .iter()
            .find(|project| project.relative_path == Path::new("app"))
            .unwrap();
        assert_eq!(app.plugin_name, "maven");
        assert_eq!(
            app.targets["build"].command,
            "./mvnw -pl :app -am package -DskipTests"
        );
        assert!(app.dependencies.is_empty());
        assert_eq!(app.targets["build"].depends_on, vec!["//app:deps"]);
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

        let shared_dir = tmp.path().join("libs/shared");
        std::fs::create_dir_all(&shared_dir).unwrap();
        std::fs::write(
            shared_dir.join("package.json"),
            r#"{"name": "shared", "version": "1.0.0", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 2);
        let api = projects
            .iter()
            .find(|project| project.metadata.name == "api-service")
            .unwrap();

        // Custom lint target from aster.toml overrides detected
        assert_eq!(
            api.targets.get("lint").map(|t| &t.command),
            Some(&"npm run lint:ci".to_string())
        );
        // Detected targets still present
        assert_eq!(
            api.targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            api.targets.get("build").map(|t| &t.command),
            Some(&"npm run build".to_string())
        );

        // depends_on should be in dependencies
        assert!(api
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
        // Total: test (detected) + deps (detected) + deploy (custom) + clean (detected) = 4
        assert_eq!(projects[0].targets.len(), 4);
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
    fn rejects_colocated_language_projects_with_the_same_address() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("app");
        std::fs::create_dir_all(project_dir.join("src")).unwrap();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"app","scripts":{"build":"echo node"}}"#,
        )
        .unwrap();
        std::fs::write(
            project_dir.join("Cargo.toml"),
            r#"[package]
name = "app-rust"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        std::fs::write(project_dir.join("src/lib.rs"), "").unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = PluginRegistry::with_all_plugins();
        let error = discover_projects(tmp.path(), &registry).unwrap_err();
        assert!(error
            .to_string()
            .contains("Multiple projects resolve to //app"));
    }

    #[test]
    fn rejects_missing_target_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("app");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"app","scripts":{"build":"echo build"}}"#,
        )
        .unwrap();
        std::fs::write(
            project_dir.join("aster.toml"),
            r#"
[targets.deploy]
command = "echo deploy"
depends_on = ["//missing:build"]
"#,
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let error = discover_projects(tmp.path(), &setup_registry()).unwrap_err();
        assert!(error
            .to_string()
            .contains("depends on missing target //missing:build"));
    }

    #[test]
    fn rejects_alias_to_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("app");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"app","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::write(
            project_dir.join("aster.toml"),
            r#"
[targets]
check = { alias = "typo" }
"#,
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let error = discover_projects(tmp.path(), &setup_registry()).unwrap_err();
        assert!(error
            .to_string()
            .contains("target //app:check depends on missing target //app:typo"));
    }

    #[test]
    fn rejects_external_alias_source() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("app");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"app","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::write(
            project_dir.join("aster.toml"),
            r#"
[targets]
check = { alias = "//other:test" }
"#,
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let error = discover_projects(tmp.path(), &setup_registry()).unwrap_err();
        assert!(error
            .to_string()
            .contains("aliases must use a local target name or //self:<target>"));
    }

    #[test]
    fn inferred_build_dependency_is_omitted_when_dependency_has_no_build_target() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_dir = tmp.path().join("packages/lib");
        let app_dir = tmp.path().join("packages/app");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            lib_dir.join("package.json"),
            r#"{"name":"lib","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            app_dir.join("package.json"),
            r#"{
  "name":"app",
  "scripts":{"test":"node --test"},
  "dependencies":{"lib":"file:../lib"}
}"#,
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let projects = discover_projects(tmp.path(), &setup_registry()).unwrap();
        let app = projects
            .iter()
            .find(|project| project.relative_path == Path::new("packages/app"))
            .unwrap();
        assert!(!app
            .targets
            .get("test")
            .unwrap()
            .depends_on
            .contains(&"//packages/lib:build".to_string()));
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

        // Python project with pytest and ruff
        let python_dir = tmp.path().join("libs/ml");
        std::fs::create_dir_all(&python_dir).unwrap();
        std::fs::write(
            python_dir.join("pyproject.toml"),
            r#"
[project]
name = "ml"
version = "1.0.0"

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
        // Python has no auto-detected build target (use aster.toml if needed)
        assert_eq!(python_project.targets.get("build"), None);
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

    #[test]
    fn test_discover_ignores_patterns() {
        let tmp = tempfile::tempdir().unwrap();

        // Create workspace aster.toml with ignore patterns
        std::fs::write(
            tmp.path().join("aster.toml"),
            r#"
ignore = [
    "vendor/**",
    "examples/**",
]
"#,
        )
        .unwrap();

        // Project that should be discovered
        let api_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("package.json"),
            r#"{"name": "api", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Project in vendor (should be ignored)
        let vendor_dir = tmp.path().join("vendor/some-lib");
        std::fs::create_dir_all(&vendor_dir).unwrap();
        std::fs::write(
            vendor_dir.join("package.json"),
            r#"{"name": "some-lib", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Project in examples (should be ignored)
        let example_dir = tmp.path().join("examples/demo");
        std::fs::create_dir_all(&example_dir).unwrap();
        std::fs::write(
            example_dir.join("package.json"),
            r#"{"name": "demo", "version": "1.0.0"}"#,
        )
        .unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        // Only the api project should be discovered
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api");
    }

    #[test]
    fn test_discover_ignores_nested_pattern() {
        let tmp = tempfile::tempdir().unwrap();

        // Ignore all node_modules anywhere
        std::fs::write(
            tmp.path().join("aster.toml"),
            r#"
ignore = ["**/node_modules/**"]
"#,
        )
        .unwrap();

        // Normal project
        let api_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("package.json"),
            r#"{"name": "api", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Project inside node_modules (should be ignored)
        let nm_dir = tmp.path().join("services/api/node_modules/some-pkg");
        std::fs::create_dir_all(&nm_dir).unwrap();
        std::fs::write(
            nm_dir.join("package.json"),
            r#"{"name": "some-pkg", "version": "1.0.0"}"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        // Only api should be discovered
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api");
    }

    #[test]
    fn test_discover_no_ignore_patterns() {
        let tmp = tempfile::tempdir().unwrap();

        // Empty aster.toml (no ignore patterns)
        std::fs::write(tmp.path().join("aster.toml"), "").unwrap();

        let api_dir = tmp.path().join("services/api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(
            api_dir.join("package.json"),
            r#"{"name": "api", "version": "1.0.0"}"#,
        )
        .unwrap();

        let registry = setup_registry();
        let projects = discover_projects(tmp.path(), &registry).unwrap();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].metadata.name, "api");
    }

    #[test]
    fn test_discover_skips_elixir_umbrella_root() {
        use crate::plugins::ElixirPlugin;

        let tmp = tempfile::tempdir().unwrap();

        // Umbrella root - should be skipped
        let umbrella_dir = tmp.path().join("src/elixir");
        std::fs::create_dir_all(&umbrella_dir).unwrap();
        std::fs::write(
            umbrella_dir.join("mix.exs"),
            r#"
defmodule ArchAstro.Umbrella.MixProject do
  use Mix.Project

  def project do
    [
      apps_path: ".",
      config_path: "umbrella.config.exs",
      start_permanent: Mix.env() == :prod,
      deps: []
    ]
  end
end
"#,
        )
        .unwrap();

        // Child app 1 - should be discovered
        let core_dir = umbrella_dir.join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(
            core_dir.join("mix.exs"),
            r#"
defmodule ArchAstroCore.MixProject do
  use Mix.Project

  def project do
    [
      app: :archastro_core,
      version: "0.1.0",
      deps: deps()
    ]
  end

  defp deps do
    [{:archastro_config, in_umbrella: true}]
  end
end
"#,
        )
        .unwrap();

        // Child app 2 - should be discovered
        let config_dir = umbrella_dir.join("config_app");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("mix.exs"),
            r#"
defmodule ArchAstroConfig.MixProject do
  use Mix.Project

  def project do
    [
      app: :archastro_config,
      version: "0.1.0"
    ]
  end
end
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(ElixirPlugin));

        let projects = discover_projects(tmp.path(), &registry).unwrap();

        // Should find 3 projects: umbrella root + 2 child apps
        assert_eq!(projects.len(), 3);

        let names: Vec<&str> = projects.iter().map(|p| p.metadata.name.as_str()).collect();
        assert!(names.contains(&"archastro_core"));
        assert!(names.contains(&"archastro_config"));
        // Umbrella root should be in the list with synthetic name
        assert!(names.contains(&"elixir_umbrella"));

        // Umbrella root should only have deps target
        let umbrella = projects
            .iter()
            .find(|p| p.metadata.name == "elixir_umbrella")
            .unwrap();
        assert_eq!(umbrella.targets.len(), 1);
        assert!(umbrella.targets.contains_key("deps"));
    }

    #[test]
    fn test_discover_umbrella_child_dependencies() {
        use crate::plugins::ElixirPlugin;

        let tmp = tempfile::tempdir().unwrap();

        // Umbrella root
        let umbrella_dir = tmp.path().join("src/elixir");
        std::fs::create_dir_all(&umbrella_dir).unwrap();
        std::fs::write(
            umbrella_dir.join("mix.exs"),
            r#"
defmodule MyUmbrella.MixProject do
  use Mix.Project
  def project do
    [apps_path: "."]
  end
end
"#,
        )
        .unwrap();

        // Child app with in_umbrella dependency
        let app_dir = umbrella_dir.join("my_app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mix.exs"),
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app, version: "0.1.0", deps: deps()]
  end

  defp deps do
    [{:my_lib, in_umbrella: true}]
  end
end
"#,
        )
        .unwrap();

        // Dependency app
        let lib_dir = umbrella_dir.join("my_lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("mix.exs"),
            r#"
defmodule MyLib.MixProject do
  use Mix.Project

  def project do
    [app: :my_lib, version: "0.1.0"]
  end
end
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(ElixirPlugin));

        let projects = discover_projects(tmp.path(), &registry).unwrap();

        // 3 projects: umbrella root + 2 child apps
        assert_eq!(projects.len(), 3);

        // Find the app that depends on the lib
        let my_app = projects
            .iter()
            .find(|p| p.metadata.name == "my_app")
            .unwrap();

        // Verify it has the in_umbrella dependency resolved to the project address
        assert_eq!(my_app.dependencies.len(), 1);
        assert_eq!(my_app.dependencies[0].name, "my_lib");
        // in_umbrella dependencies are resolved to project addresses (//path)
        assert_eq!(
            my_app.dependencies[0].path,
            std::path::PathBuf::from("//src/elixir/my_lib")
        );
    }

    #[test]
    fn test_discover_umbrella_mismatched_names() {
        use crate::plugins::ElixirPlugin;

        let tmp = tempfile::tempdir().unwrap();

        // Umbrella root
        let umbrella_dir = tmp.path().join("src/elixir");
        std::fs::create_dir_all(&umbrella_dir).unwrap();
        std::fs::write(
            umbrella_dir.join("mix.exs"),
            r#"
defmodule MyUmbrella.MixProject do
  use Mix.Project
  def project do
    [apps_path: "."]
  end
end
"#,
        )
        .unwrap();

        // Child app depending on app with different directory name
        let app_dir = umbrella_dir.join("core");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mix.exs"),
            r#"
defmodule Core.MixProject do
  use Mix.Project

  def project do
    [app: :archastro_core, version: "0.1.0", deps: deps()]
  end

  defp deps do
    [{:archastro_config, in_umbrella: true}]
  end
end
"#,
        )
        .unwrap();

        // Dependency app with different directory name than app name
        // Directory: "config" but app: :archastro_config
        let config_dir = umbrella_dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("mix.exs"),
            r#"
defmodule Config.MixProject do
  use Mix.Project

  def project do
    [app: :archastro_config, version: "0.1.0"]
  end
end
"#,
        )
        .unwrap();

        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(ElixirPlugin));

        let projects = discover_projects(tmp.path(), &registry).unwrap();

        // 3 projects: umbrella root + 2 child apps
        assert_eq!(projects.len(), 3);

        // Find the core app
        let core = projects
            .iter()
            .find(|p| p.metadata.name == "archastro_core")
            .unwrap();

        // Verify in_umbrella dependency is resolved by app name, not directory name
        assert_eq!(core.dependencies.len(), 1);
        assert_eq!(core.dependencies[0].name, "archastro_config");
        // Resolved to the actual project path (config directory)
        assert_eq!(
            core.dependencies[0].path,
            std::path::PathBuf::from("//src/elixir/config")
        );
    }

    #[test]
    fn discovers_ruby_sibling_gems_and_resolves_runtime_dependencies_by_name() {
        use crate::plugins::RubyPlugin;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        for (directory, name, dependency) in [
            ("gems/shared", "shared", None),
            ("gems/app", "app", Some("shared")),
        ] {
            let project = tmp.path().join(directory);
            std::fs::create_dir_all(project.join("test")).unwrap();
            std::fs::write(project.join("Gemfile"), "gemspec\n").unwrap();
            std::fs::write(
                project.join(format!("{name}.gemspec")),
                format!(
                    "Gem::Specification.new do |s|\n  s.name = '{name}'\n{}end\n",
                    dependency
                        .map(|value| format!("  s.add_dependency '{value}'\n"))
                        .unwrap_or_default()
                ),
            )
            .unwrap();
            std::fs::write(
                project.join("Rakefile"),
                "require 'rake/testtask'\nRake::TestTask.new\n",
            )
            .unwrap();
        }

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RubyPlugin));
        let projects = discover_projects(tmp.path(), &registry).unwrap();
        assert_eq!(projects.len(), 2);
        let app = projects
            .iter()
            .find(|project| project.metadata.name == "app")
            .unwrap();
        assert_eq!(app.dependencies.len(), 1);
        assert_eq!(app.dependencies[0].path, Path::new("//gems/shared"));
        assert_eq!(app.targets["test"].command, "bundle exec rake test");
    }
}
