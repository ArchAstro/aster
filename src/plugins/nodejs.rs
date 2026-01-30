//! Node.js language plugin for package.json parsing

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{
    LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetCapability, TargetContext,
};

/// Internal representation of package.json for serde deserialization
#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
    scripts: Option<HashMap<String, String>>,
    workspaces: Option<WorkspacesConfig>,
}

/// npm workspaces can be either an array of glob patterns or an object with a `packages` field
#[derive(Deserialize)]
#[serde(untagged)]
enum WorkspacesConfig {
    /// Simple array: `"workspaces": ["packages/*"]`
    Patterns(Vec<String>),
    /// Object form: `"workspaces": { "packages": ["packages/*"] }`
    Object { packages: Vec<String> },
}

impl WorkspacesConfig {
    fn patterns(&self) -> &[String] {
        match self {
            WorkspacesConfig::Patterns(p) => p,
            WorkspacesConfig::Object { packages } => packages,
        }
    }
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
        for dep_map in [pkg.dependencies, pkg.dev_dependencies]
            .into_iter()
            .flatten()
        {
            for (name, version) in dep_map {
                if let Some(path_str) = version.strip_prefix("file:") {
                    let path = project_dir.join(path_str);
                    deps.push(LocalDependency { name, path });
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

        // Check if this project is a member of an npm workspace.
        // Workspace members share a single node_modules/ at the workspace root,
        // so we skip the per-package deps target entirely and point all dependency
        // references directly at the workspace root's deps target.
        let workspace_root_addr = find_npm_workspace_root(ctx);

        // The deps reference that other targets in this project should depend on.
        let deps_ref = if let Some(ref ws_addr) = workspace_root_addr {
            // Workspace member: depend directly on the workspace root's deps
            format!("{ws_addr}:deps")
        } else {
            // Standalone project or workspace root: run npm install as //self:deps
            targets.insert(
                "deps".to_string(),
                Target {
                    command: "npm install".to_string(),
                    depends_on: vec![],
                    capabilities: HashSet::new(),
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                },
            );
            "//self:deps".to_string()
        };

        // Resolve dependency paths to project addresses
        let dependency_addresses = resolve_dependency_addresses(ctx);

        // Build dependencies for non-deps targets:
        // - deps_ref (install dependencies first — either our own or workspace root's)
        // - :build for each project dependency (they must be built first)
        let mut base_deps = vec![deps_ref.clone()];
        for dep_addr in &dependency_addresses {
            base_deps.push(format!("{dep_addr}:build"));
        }

        let format_deps = vec![deps_ref.clone(), "//self:build".to_string()];

        if let Some(scripts) = pkg.scripts {
            // Track scripts to skip entirely (e.g., npm placeholder scripts)
            let mut skip_scripts: HashSet<&str> = HashSet::new();

            // Map npm scripts to aster targets
            // Only add targets for scripts that actually exist
            // Skip npm's default "no test specified" placeholder
            if let Some(test_script) = scripts.get("test") {
                let is_placeholder = test_script.contains("no test specified")
                    || test_script.contains("Error: no test");
                if is_placeholder {
                    skip_scripts.insert("test");
                } else {
                    let mut test_caps = HashSet::new();
                    test_caps.insert(TargetCapability::FilesList);
                    targets.insert(
                        "test".to_string(),
                        Target {
                            command: "npm test".to_string(),
                            depends_on: base_deps.clone(),
                            capabilities: test_caps,
                            files_glob: None,
                            stream: false,
                            cache: None,
                            invalidates_cache: false,
                            working_dir: None,
                        },
                    );
                }
            }
            if scripts.contains_key("build") {
                targets.insert(
                    "build".to_string(),
                    Target {
                        command: "npm run build".to_string(),
                        depends_on: base_deps.clone(),
                        capabilities: HashSet::new(),
                        files_glob: None,
                        stream: false,
                        cache: None,
                        invalidates_cache: false,
                        working_dir: None,
                    },
                );
            }
            if scripts.contains_key("lint") {
                targets.insert(
                    "lint".to_string(),
                    Target {
                        command: "npm run lint".to_string(),
                        depends_on: base_deps.clone(),
                        capabilities: HashSet::new(),
                        files_glob: None,
                        stream: false,
                        cache: None,
                        invalidates_cache: false,
                        working_dir: None,
                    },
                );
            }
            if scripts.contains_key("format") {
                targets.insert(
                    "format".to_string(),
                    Target {
                        command: "npm run format".to_string(),
                        depends_on: format_deps.clone(),
                        capabilities: HashSet::new(),
                        files_glob: None,
                        stream: false,
                        cache: None,
                        invalidates_cache: false,
                        working_dir: None,
                    },
                );
            }
            // Also map any other scripts as targets (with same dependencies)
            for (script_name, _) in scripts {
                if !targets.contains_key(&script_name)
                    && !skip_scripts.contains(script_name.as_str())
                {
                    targets.insert(
                        script_name.clone(),
                        Target {
                            command: format!("npm run {script_name}"),
                            depends_on: base_deps.clone(),
                            capabilities: HashSet::new(),
                            files_glob: None,
                            stream: false,
                            cache: None,
                            invalidates_cache: false,
                            working_dir: None,
                        },
                    );
                }
            }
        }

        // If no format script, check for Prettier config
        if !targets.contains_key("format") {
            let prettier_configs = [
                ".prettierrc",
                ".prettierrc.json",
                ".prettierrc.js",
                ".prettierrc.cjs",
                ".prettierrc.mjs",
                ".prettierrc.yml",
                ".prettierrc.yaml",
                ".prettierrc.toml",
                "prettier.config.js",
                "prettier.config.cjs",
                "prettier.config.mjs",
            ];
            let has_prettier = prettier_configs
                .iter()
                .any(|config| ctx.project_dir.join(config).exists());

            if has_prettier {
                targets.insert(
                    "format".to_string(),
                    Target {
                        command: "npx prettier --write .".to_string(),
                        depends_on: format_deps,
                        capabilities: HashSet::new(),
                        files_glob: None,
                        stream: false,
                        cache: None,
                        invalidates_cache: false,
                        working_dir: None,
                    },
                );
            }
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

        // Filter to test files only (*.test.*, *.spec.*, __tests__/*)
        let test_files: Vec<&PathBuf> = files
            .iter()
            .filter(|f| {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let path_str = f.to_string_lossy();
                name.contains(".test.")
                    || name.contains(".spec.")
                    || name.contains("_test.")
                    || path_str.contains("__tests__")
                    || path_str.contains("tests/")
            })
            .collect();

        if test_files.is_empty() {
            // No test files in the change set - run full test suite
            return None;
        }

        // npm test -- file1 file2 (-- passes args through to the underlying test runner)
        let file_args: Vec<String> = test_files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect();

        Some(format!("{} -- {}", command, file_args.join(" ")))
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        let mut inputs = super::CacheInputs {
            source_globs: vec![
                "src/**/*.ts".to_string(),
                "src/**/*.tsx".to_string(),
                "src/**/*.js".to_string(),
                "src/**/*.jsx".to_string(),
                "lib/**/*.ts".to_string(),
                "lib/**/*.js".to_string(),
            ],
            config_files: vec![
                "package.json".to_string(),
                "package-lock.json".to_string(),
                "yarn.lock".to_string(),
                "pnpm-lock.yaml".to_string(),
                "tsconfig.json".to_string(),
            ],
            env_vars: vec!["NODE_ENV".to_string(), "CI".to_string()],
        };

        // Add test files for test target
        if target_name == "test" {
            inputs.source_globs.push("test/**/*.ts".to_string());
            inputs.source_globs.push("test/**/*.js".to_string());
            inputs.source_globs.push("__tests__/**/*.ts".to_string());
            inputs.source_globs.push("__tests__/**/*.js".to_string());
            inputs.source_globs.push("**/*.test.ts".to_string());
            inputs.source_globs.push("**/*.spec.ts".to_string());
        }

        inputs
    }

    fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
        let mut dirs_to_clean = vec!["node_modules"];

        // Detect framework-specific directories
        if ctx.project_dir.join(".next").exists() {
            dirs_to_clean.push(".next");
        }
        if ctx.project_dir.join("dist").exists() {
            dirs_to_clean.push("dist");
        }
        if ctx.project_dir.join(".turbo").exists() {
            dirs_to_clean.push(".turbo");
        }
        if ctx.project_dir.join("build").exists() {
            dirs_to_clean.push("build");
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
        })
    }
}

/// Detect if this project is a member of an npm workspace.
///
/// Walks up from `project_dir` toward `workspace_root` looking for a parent
/// `package.json` with a `workspaces` field whose glob patterns include this project.
///
/// Returns the aster project address of the npm workspace root (e.g., `//src/ts/native-templates`)
/// if this project is a workspace member, or None if it's standalone.
fn find_npm_workspace_root(ctx: &TargetContext) -> Option<String> {
    let project_dir = ctx.project_dir;

    // Walk up from project_dir, stopping before workspace_root
    let mut candidate = project_dir.parent()?;
    while candidate.starts_with(ctx.workspace_root) && candidate != ctx.workspace_root {
        let parent_pkg = candidate.join("package.json");
        if parent_pkg.exists() {
            if let Ok(content) = std::fs::read_to_string(&parent_pkg) {
                if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
                    if let Some(ref ws) = pkg.workspaces {
                        // Check if our project matches one of the workspace patterns
                        let relative = project_dir.strip_prefix(candidate).ok()?;
                        let relative_str = relative.to_string_lossy();

                        for pattern in ws.patterns() {
                            if matches_workspace_pattern(pattern, &relative_str) {
                                // Return aster address of the npm workspace root
                                let ws_relative =
                                    candidate.strip_prefix(ctx.workspace_root).ok()?;
                                return Some(format!("//{}", ws_relative.display()));
                            }
                        }
                    }
                }
            }
        }
        candidate = candidate.parent()?;
    }

    // Also check the workspace root itself
    let root_pkg = ctx.workspace_root.join("package.json");
    if root_pkg.exists() && ctx.project_dir != ctx.workspace_root {
        if let Ok(content) = std::fs::read_to_string(&root_pkg) {
            if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
                if let Some(ref ws) = pkg.workspaces {
                    let relative = project_dir.strip_prefix(ctx.workspace_root).ok()?;
                    let relative_str = relative.to_string_lossy();

                    for pattern in ws.patterns() {
                        if matches_workspace_pattern(pattern, &relative_str) {
                            // Workspace root is at the aster workspace root
                            return Some("//".to_string());
                        }
                    }
                }
            }
        }
    }

    None
}

/// Check if a relative path matches an npm workspace glob pattern.
///
/// npm workspace patterns use simple globs: `packages/*`, `apps/*`, or exact paths.
/// `*` matches a single directory segment (not recursive).
fn matches_workspace_pattern(pattern: &str, relative_path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = relative_path.split('/').filter(|s| !s.is_empty()).collect();

    if pattern_parts.len() != path_parts.len() {
        return false;
    }

    pattern_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(pat, path)| *pat == "*" || *pat == *path)
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
        std::fs::write(&pkg_json, r#"{"name": "my-app", "version": "1.2.3"}"#).unwrap();

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
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"npm run build".to_string())
        );
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"npm run lint".to_string())
        );

        // Check that deps target exists
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );

        // Check dependencies (//self:deps)
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//self:deps"]);
        assert_eq!(
            targets.get("build").unwrap().depends_on,
            vec!["//self:deps"]
        );
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
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );
        // build and lint are not present (no scripts for them)
        assert_eq!(targets.get("build"), None);
        assert_eq!(targets.get("lint"), None);
    }

    #[test]
    fn test_detect_targets_no_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app", "version": "1.0.0"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // deps and clean targets are always present
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );
        assert!(targets.get("clean").is_some());
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
        assert_eq!(
            targets.get("deploy").map(|t| &t.command),
            Some(&"npm run deploy".to_string())
        );
        assert_eq!(
            targets.get("validate").map(|t| &t.command),
            Some(&"npm run validate".to_string())
        );
        assert_eq!(
            targets.get("deploy").unwrap().depends_on,
            vec!["//self:deps"]
        );
        // Standard targets not present (not in scripts)
        assert_eq!(targets.get("test"), None);
        assert_eq!(targets.get("build"), None);
        // deps is always present
        assert!(targets.get("deps").is_some());
    }

    #[test]
    fn test_detect_targets_format_script() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"format": "prettier --write ."}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses npm run format
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npm run format".to_string())
        );

        // Check dependencies
        let format_deps = &targets.get("format").unwrap().depends_on;
        assert!(format_deps.contains(&"//self:deps".to_string()));
        assert!(format_deps.contains(&"//self:build".to_string()));
        assert_eq!(format_deps.len(), 2);
    }

    #[test]
    fn test_detect_targets_format_prettier_config() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        // Create .prettierrc
        std::fs::write(tmp.path().join(".prettierrc"), r#"{"semi": false}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses npx prettier
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npx prettier --write .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_format_prettier_config_js() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        // Create prettier.config.js
        std::fs::write(
            tmp.path().join("prettier.config.js"),
            "module.exports = { semi: false };",
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses npx prettier
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npx prettier --write .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_format_script_preferred_over_prettier() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"format": "biome format --write ."}}"#,
        )
        .unwrap();

        // Also create .prettierrc
        std::fs::write(tmp.path().join(".prettierrc"), r#"{"semi": false}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format script takes priority over prettier config
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npm run format".to_string())
        );
    }

    #[test]
    fn test_detect_targets_no_format_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // no format target (no script or prettier config)
        assert_eq!(targets.get("format"), None);
    }

    #[test]
    fn test_with_files_list_filters_test_files() {
        let plugin = NodeJsPlugin;
        let files = vec![
            PathBuf::from("src/index.ts"),
            PathBuf::from("src/utils.test.ts"),
            PathBuf::from("src/helpers.spec.js"),
            PathBuf::from("__tests__/integration.js"),
            PathBuf::from("package.json"),
        ];

        let result = plugin.with_files_list("test", "npm test", &files);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.starts_with("npm test -- "));
        assert!(cmd.contains("utils.test.ts"));
        assert!(cmd.contains("helpers.spec.js"));
        assert!(cmd.contains("__tests__/integration.js"));
        assert!(!cmd.contains("index.ts"));
        assert!(!cmd.contains("package.json"));
    }

    #[test]
    fn test_with_files_list_returns_none_for_non_test_target() {
        let plugin = NodeJsPlugin;
        let files = vec![PathBuf::from("src/index.test.ts")];

        let result = plugin.with_files_list("build", "npm run build", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_returns_none_when_no_test_files() {
        let plugin = NodeJsPlugin;
        let files = vec![
            PathBuf::from("src/index.ts"),
            PathBuf::from("src/utils.ts"),
            PathBuf::from("package.json"),
        ];

        let result = plugin.with_files_list("test", "npm test", &files);

        // No test files, so run full suite
        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_empty_files() {
        let plugin = NodeJsPlugin;
        let files: Vec<PathBuf> = vec![];

        let result = plugin.with_files_list("test", "npm test", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_test_target_has_files_list_capability() {
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

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));

        // build should not have the capability
        let deps_target = targets.get("deps").unwrap();
        assert!(!deps_target
            .capabilities
            .contains(&TargetCapability::FilesList));
    }

    #[test]
    fn test_skip_npm_placeholder_test_script() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        // This is the default npm init test script that should be ignored
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test target should NOT be created for placeholder script
        assert_eq!(targets.get("test"), None);

        // deps is always present
        assert!(targets.get("deps").is_some());
    }

    #[test]
    fn test_skip_npm_placeholder_test_script_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        // Another variant of the placeholder
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "echo 'Error: no test' && exit 1"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test target should NOT be created for placeholder script
        assert_eq!(targets.get("test"), None);
    }

    #[test]
    fn test_detect_targets_has_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert_eq!(clean.command, "rm -rf node_modules");
        assert!(clean.depends_on.is_empty());
        assert!(clean.invalidates_cache);
    }

    #[test]
    fn test_clean_target_detects_next() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        // Create .next directory
        std::fs::create_dir(tmp.path().join(".next")).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert!(clean.command.contains("node_modules"));
        assert!(clean.command.contains(".next"));
    }

    #[test]
    fn test_matches_workspace_pattern_glob() {
        assert!(matches_workspace_pattern("packages/*", "packages/core"));
        assert!(matches_workspace_pattern("packages/*", "packages/react"));
        assert!(!matches_workspace_pattern("packages/*", "apps/web"));
        assert!(!matches_workspace_pattern(
            "packages/*",
            "packages/deep/nested"
        ));
    }

    #[test]
    fn test_matches_workspace_pattern_exact() {
        assert!(matches_workspace_pattern(
            "apps/storybook-react",
            "apps/storybook-react"
        ));
        assert!(!matches_workspace_pattern(
            "apps/storybook-react",
            "apps/web"
        ));
    }

    #[test]
    fn test_workspace_member_has_no_deps_target() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root with workspaces field
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member
        let member_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/core", "scripts": {"build": "tsc", "test": "vitest"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&member_pkg, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // No deps target — workspace root handles installation
        assert!(targets.get("deps").is_none());

        // Other targets depend directly on the workspace root's deps
        let build = targets.get("build").unwrap();
        assert!(build.depends_on.contains(&"//:deps".to_string()));
        let test = targets.get("test").unwrap();
        assert!(test.depends_on.contains(&"//:deps".to_string()));
    }

    #[test]
    fn test_workspace_root_runs_npm_install() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root
        let pkg_json = workspace_root.join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "monorepo", "workspaces": ["packages/*"], "scripts": {"build": "npm run build --workspaces"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Workspace root itself should run npm install directly
        let deps = targets.get("deps").unwrap();
        assert_eq!(deps.command, "npm install");
        assert!(deps.depends_on.is_empty());
    }

    #[test]
    fn test_workspace_member_nested_root() {
        let tmp = tempfile::tempdir().unwrap();
        let aster_root = tmp.path().canonicalize().unwrap();

        // npm workspace root is nested under aster workspace root
        let npm_root = aster_root.join("src/ts/native-templates");
        std::fs::create_dir_all(&npm_root).unwrap();
        std::fs::write(
            npm_root.join("package.json"),
            r#"{"name": "native-templates", "workspaces": ["packages/*", "apps/storybook-react"]}"#,
        )
        .unwrap();

        // Create workspace member
        let member_dir = npm_root.join("packages/react");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/react", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&member_pkg, &aster_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // No deps target on the member
        assert!(targets.get("deps").is_none());

        // build depends directly on the nested npm workspace root's deps
        let build = targets.get("build").unwrap();
        assert!(build
            .depends_on
            .contains(&"//src/ts/native-templates:deps".to_string()));
    }

    #[test]
    fn test_non_workspace_member_not_affected() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root with specific patterns
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create a project NOT matching workspace patterns
        let standalone_dir = workspace_root.join("tools/cli");
        std::fs::create_dir_all(&standalone_dir).unwrap();
        let standalone_pkg = standalone_dir.join("package.json");
        std::fs::write(
            &standalone_pkg,
            r#"{"name": "cli-tool", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&standalone_pkg, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Should run its own npm install since it's not a workspace member
        let deps = targets.get("deps").unwrap();
        assert_eq!(deps.command, "npm install");
        assert!(deps.depends_on.is_empty());
    }

    #[test]
    fn test_workspace_object_form() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // npm also supports object form: { "packages": [...] }
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": {"packages": ["packages/*"]}}"#,
        )
        .unwrap();

        let member_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/core", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&member_pkg, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // No deps target on the member
        assert!(targets.get("deps").is_none());

        // build depends directly on workspace root's deps
        let build = targets.get("build").unwrap();
        assert!(build.depends_on.contains(&"//:deps".to_string()));
    }
}
