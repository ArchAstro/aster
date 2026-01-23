//! Node.js language plugin for package.json parsing

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use super::{LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetContext};

/// Internal representation of package.json for serde deserialization
#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
    scripts: Option<HashMap<String, String>>,
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

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let content = std::fs::read_to_string(ctx.config_path)
            .with_context(|| format!("Failed to read {}", ctx.config_path.display()))?;

        let pkg: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", ctx.config_path.display()))?;

        let mut targets = HashMap::new();

        // Always add deps target for npm install (no cross-project dependencies)
        targets.insert(
            "deps".to_string(),
            Target {
                command: "npm install".to_string(),
                depends_on: vec![],
            },
        );

        // Resolve dependency paths to project addresses
        let dependency_addresses = resolve_dependency_addresses(ctx);

        // Build dependencies for non-deps targets:
        // - //self:deps (install our own dependencies first)
        // - :build for each project dependency (they must be built first)
        let mut base_deps = vec!["//self:deps".to_string()];
        for dep_addr in &dependency_addresses {
            base_deps.push(format!("{}:build", dep_addr));
        }

        if let Some(scripts) = pkg.scripts {
            // Map npm scripts to aster targets
            // Only add targets for scripts that actually exist
            if scripts.contains_key("test") {
                targets.insert(
                    "test".to_string(),
                    Target {
                        command: "npm test".to_string(),
                        depends_on: base_deps.clone(),
                    },
                );
            }
            if scripts.contains_key("build") {
                targets.insert(
                    "build".to_string(),
                    Target {
                        command: "npm run build".to_string(),
                        depends_on: base_deps.clone(),
                    },
                );
            }
            if scripts.contains_key("lint") {
                targets.insert(
                    "lint".to_string(),
                    Target {
                        command: "npm run lint".to_string(),
                        depends_on: base_deps.clone(),
                    },
                );
            }
            // Also map any other scripts as targets (with same dependencies)
            for (script_name, _) in scripts {
                if !targets.contains_key(&script_name) {
                    targets.insert(
                        script_name.clone(),
                        Target {
                            command: format!("npm run {}", script_name),
                            depends_on: base_deps.clone(),
                        },
                    );
                }
            }
        }

        Ok(targets)
    }
}

/// Resolve LocalDependency paths to project addresses
///
/// Handles both address strings (//libs/shared) and relative paths (../../libs/shared)
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
    fn test_detect_targets_with_all_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest", "build": "tsc", "lint": "eslint ."}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Check commands
        assert_eq!(targets.get("test").map(|t| &t.command), Some(&"npm test".to_string()));
        assert_eq!(targets.get("build").map(|t| &t.command), Some(&"npm run build".to_string()));
        assert_eq!(targets.get("lint").map(|t| &t.command), Some(&"npm run lint".to_string()));

        // Check that deps target exists
        assert_eq!(targets.get("deps").map(|t| &t.command), Some(&"npm install".to_string()));

        // Check dependencies (//self:deps)
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//self:deps"]);
        assert_eq!(targets.get("build").unwrap().depends_on, vec!["//self:deps"]);
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
    }

    #[test]
    fn test_detect_targets_with_project_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize workspace root to handle macOS /var -> /private/var symlink
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create workspace structure
        let project_dir = workspace_root.join("apps/web");
        std::fs::create_dir_all(&project_dir).unwrap();
        let pkg_json = project_dir.join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest", "build": "tsc"}}"#,
        )
        .unwrap();

        // Create dependency directories (needed for canonicalize)
        let core_dir = workspace_root.join("libs/core");
        let utils_dir = workspace_root.join("libs/utils");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::create_dir_all(&utils_dir).unwrap();

        // Dependencies with relative paths
        let dependencies = vec![
            LocalDependency {
                name: "core".to_string(),
                path: "../../libs/core".into(),
            },
            LocalDependency {
                name: "utils".to_string(),
                path: "../../libs/utils".into(),
            },
        ];

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, &workspace_root, &dependencies);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test should depend on deps and build of each dependency
        let test_deps = &targets.get("test").unwrap().depends_on;
        assert!(test_deps.contains(&"//self:deps".to_string()));
        assert!(test_deps.contains(&"//libs/core:build".to_string()));
        assert!(test_deps.contains(&"//libs/utils:build".to_string()));

        // build should also depend on deps and build of each dependency
        let build_deps = &targets.get("build").unwrap().depends_on;
        assert!(build_deps.contains(&"//self:deps".to_string()));
        assert!(build_deps.contains(&"//libs/core:build".to_string()));
        assert!(build_deps.contains(&"//libs/utils:build".to_string()));

        // deps should NOT depend on project dependencies
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
    }

    #[test]
    fn test_detect_targets_with_some_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test and deps are present
        assert_eq!(targets.get("test").map(|t| &t.command), Some(&"npm test".to_string()));
        assert_eq!(targets.get("deps").map(|t| &t.command), Some(&"npm install".to_string()));
        // build and lint are not present (no scripts for them)
        assert_eq!(targets.get("build"), None);
        assert_eq!(targets.get("lint"), None);
    }

    #[test]
    fn test_detect_targets_no_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "version": "1.0.0"}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // deps target is always present
        assert_eq!(targets.len(), 1);
        assert_eq!(targets.get("deps").map(|t| &t.command), Some(&"npm install".to_string()));
    }

    #[test]
    fn test_detect_targets_custom_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"deploy": "npm publish", "validate": "npm run test && npm run lint"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Custom scripts are mapped (with deps dependency)
        assert_eq!(targets.get("deploy").map(|t| &t.command), Some(&"npm run deploy".to_string()));
        assert_eq!(targets.get("validate").map(|t| &t.command), Some(&"npm run validate".to_string()));
        assert_eq!(targets.get("deploy").unwrap().depends_on, vec!["//self:deps"]);
        // Standard targets not present (not in scripts)
        assert_eq!(targets.get("test"), None);
        assert_eq!(targets.get("build"), None);
        // deps is always present
        assert!(targets.get("deps").is_some());
    }
}
