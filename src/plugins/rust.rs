//! Rust language plugin for Cargo.toml parsing

use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use toml::Value;

use super::{
    FilesListPlan, FilesListSelection, LanguagePlugin, LocalDependency, NativeCache,
    ProjectMetadata, Target, TargetCapability, TargetContext,
};

/// Rust plugin for discovering and parsing Cargo projects
pub struct RustPlugin;

impl LanguagePlugin for RustPlugin {
    fn name(&self) -> &str {
        "rust"
    }

    fn marker_files(&self) -> &[&str] {
        &["Cargo.toml"]
    }

    fn should_skip(&self, config_path: &Path) -> bool {
        // A virtual workspace manifest has `[workspace]` and no `[package]`.
        // It is not a buildable project; its members are. Leaving it for
        // `parse_project` aborts discovery of the whole workspace.
        // A member that also declares `[workspace]` has a `[package]` and
        // is not skipped.
        match std::fs::read_to_string(config_path) {
            Ok(content) => is_virtual_workspace_manifest(&content),
            Err(_) => false,
        }
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let cargo_toml: Value = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        // Extract package name and version
        let package = cargo_toml.get("package").ok_or_else(|| {
            anyhow!(
                "No [package] section in {} (workspace roots are not projects)",
                config_path.display()
            )
        })?;

        let name = package
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing 'name' in [package] section"))?
            .to_string();

        let version = package
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(ProjectMetadata { name, version })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let cargo_toml: Value = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let mut deps = Vec::new();

        // Check both [dependencies] and [dev-dependencies] for path dependencies
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dep_table) = cargo_toml.get(section).and_then(|v| v.as_table()) {
                for (name, value) in dep_table {
                    // Path dependencies can be:
                    // dep = { path = "../foo" }
                    // or with other fields like version, features, etc.
                    if let Some(path_str) = value
                        .as_table()
                        .and_then(|t| t.get("path"))
                        .and_then(|v| v.as_str())
                    {
                        deps.push(LocalDependency {
                            name: name.clone(),
                            path: PathBuf::from(path_str),
                        });
                    }
                }
            }
        }

        Ok(deps)
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let mut targets = HashMap::new();

        // deps target: fetch dependencies (cargo fetch is optional, cargo build does this)
        // Using empty command since cargo handles deps automatically
        targets.insert(
            "deps".to_string(),
            Target {
                command: "cargo fetch".to_string(),
                depends_on: vec![],
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // Resolve dependency paths to project addresses
        let dependency_addresses = resolve_dependency_addresses(ctx);

        // Build dependencies for non-deps targets:
        // - //self:deps (fetch dependencies first)
        // - :build for each path dependency (they must be built first)
        let mut base_deps = vec!["//self:deps".to_string()];
        for dep_addr in &dependency_addresses {
            base_deps.push(format!("{dep_addr}:build"));
        }

        // build target
        targets.insert(
            "build".to_string(),
            Target {
                command: "cargo build".to_string(),
                depends_on: base_deps.clone(),
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // check target (fast type checking)
        targets.insert(
            "check".to_string(),
            Target {
                command: "cargo check".to_string(),
                depends_on: base_deps.clone(),
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // test target: `--only-affected-files` narrows it to related tests
        targets.insert(
            "test".to_string(),
            Target {
                command: "cargo test".to_string(),
                depends_on: base_deps.clone(),
                capabilities: HashSet::from([TargetCapability::FilesList]),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // lint target (clippy)
        targets.insert(
            "lint".to_string(),
            Target {
                command: "cargo clippy".to_string(),
                depends_on: base_deps.clone(),
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // format target
        targets.insert(
            "format".to_string(),
            Target {
                command: "cargo fmt".to_string(),
                depends_on: vec!["//self:deps".to_string()],
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // Add clean target
        if let Some(clean) = self.clean_target(ctx) {
            targets.insert("clean".to_string(), clean);
        }

        Ok(targets)
    }

    /// Narrow a `cargo test` command to the tests related to `files`; see
    /// `rust_related` for the selection rules. Other commands run unchanged.
    fn select_for_files(
        &self,
        project_dir: &Path,
        _target_name: &str,
        command: &str,
        files: &[PathBuf],
    ) -> FilesListSelection {
        if files.is_empty() {
            return FilesListSelection::new(FilesListPlan::Nothing);
        }
        super::rust_related::select(project_dir, command, files)
    }

    fn related_tests(
        &self,
        project_dir: &Path,
        command: &str,
        tests: &[super::RelatedTest],
    ) -> Option<FilesListPlan> {
        super::rust_related::related(project_dir, command, tests)
    }

    fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
        Some(Target {
            command: "cargo clean".to_string(),
            depends_on: vec![],
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: true,
            working_dir: None,
            exclusive_resources: vec![],
            retry_setup: None,
        })
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        // deps target only cares about dependency specification files,
        // not source code — source changes shouldn't re-fetch dependencies
        if target_name == "deps" {
            return super::CacheInputs {
                source_globs: vec![],
                config_files: vec!["Cargo.toml".to_string(), "Cargo.lock".to_string()],
                env_vars: vec![],
                ..Default::default()
            };
        }

        let mut inputs = super::CacheInputs {
            source_globs: vec!["src/**/*.rs".to_string(), "build.rs".to_string()],
            config_files: vec![
                "Cargo.toml".to_string(),
                "Cargo.lock".to_string(),
                "rust-toolchain.toml".to_string(),
                ".cargo/config.toml".to_string(),
            ],
            env_vars: vec!["RUSTFLAGS".to_string(), "CARGO_TARGET_DIR".to_string()],
            ..Default::default()
        };

        // Add test files for test target
        if target_name == "test" {
            inputs.source_globs.push("tests/**/*.rs".to_string());
            inputs.source_globs.push("benches/**/*.rs".to_string());
        }

        inputs
    }

    fn workspace_cache_inputs(
        &self,
        project_root: &Path,
        workspace_root: &Path,
        target_name: &str,
    ) -> Vec<String> {
        if project_root == workspace_root {
            return Vec::new();
        }
        if !is_cargo_workspace_member(project_root, workspace_root) {
            return Vec::new();
        }
        // Cargo reads the workspace Cargo.toml and Cargo.lock from the
        // workspace root, not from the member. Toolchain and cargo config
        // change what a build produces, but not what `cargo fetch` resolves.
        let mut files = vec!["Cargo.toml".to_string(), "Cargo.lock".to_string()];
        if target_name != "deps" {
            files.push("rust-toolchain.toml".to_string());
            files.push(".cargo/config.toml".to_string());
        }
        files
    }

    fn native_caches(&self, ctx: &TargetContext) -> Vec<NativeCache> {
        let cargo_root = cargo_workspace_root(ctx.project_dir, ctx.workspace_root);
        let lock_file = workspace_relative(&cargo_root.join("Cargo.lock"), ctx.workspace_root);
        let target_dir = workspace_relative(&cargo_root.join("target"), ctx.workspace_root);
        let root_rel = workspace_relative(&cargo_root, ctx.workspace_root);
        let target_id = if root_rel.is_empty() {
            "cargo-target".to_string()
        } else {
            format!("cargo-target:{root_rel}")
        };

        vec![
            NativeCache {
                id: "cargo-registry".to_string(),
                paths: vec![
                    "~/.cargo/registry/index".to_string(),
                    "~/.cargo/registry/cache".to_string(),
                    "~/.cargo/git/db".to_string(),
                ],
                key_files: vec![lock_file.clone()],
                key_runtime: None,
                shared: true,
            },
            NativeCache {
                id: target_id,
                paths: vec![target_dir],
                key_files: vec![lock_file],
                key_runtime: Some("rustc -vV".to_string()),
                shared: false,
            },
        ]
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

/// True when `content` is a virtual workspace manifest: `[workspace]` and
/// no `[package]`. A real crate that also declares `[workspace]` is a project.
fn is_virtual_workspace_manifest(content: &str) -> bool {
    let Ok(value) = toml::from_str::<Value>(content) else {
        return false;
    };
    value.get("workspace").is_some() && value.get("package").is_none()
}

/// True when `project_root` is a member of the Cargo workspace at
/// `workspace_root`.
///
/// Membership is the root `[workspace]` table: `members` matches the
/// crate's path, `exclude` does not, and the crate has no `[workspace]`
/// of its own. A nested workspace is its own root, so its lockfile is the
/// one Cargo uses.
fn is_cargo_workspace_member(project_root: &Path, workspace_root: &Path) -> bool {
    let Ok(relative) = project_root.strip_prefix(workspace_root) else {
        return false;
    };
    if relative.as_os_str().is_empty() {
        return false;
    }
    let relative = relative.to_string_lossy().replace('\\', "/");
    let Ok(root_manifest) = std::fs::read_to_string(workspace_root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(root) = toml::from_str::<Value>(&root_manifest) else {
        return false;
    };
    let Some(workspace) = root.get("workspace").and_then(|value| value.as_table()) else {
        return false;
    };
    let Some(members) = workspace.get("members").and_then(|value| value.as_array()) else {
        return false;
    };
    let excluded = workspace
        .get("exclude")
        .and_then(|value| value.as_array())
        .is_some_and(|exclude| {
            exclude
                .iter()
                .filter_map(|value| value.as_str())
                .any(|pattern| cargo_member_matches(&relative, pattern))
        });
    if excluded {
        return false;
    }
    if !members
        .iter()
        .filter_map(|value| value.as_str())
        .any(|pattern| cargo_member_matches(&relative, pattern))
    {
        return false;
    }
    let Ok(member_manifest) = std::fs::read_to_string(project_root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(member) = toml::from_str::<Value>(&member_manifest) else {
        return false;
    };
    member.get("workspace").is_none()
}

/// Cargo `members`/`exclude` patterns: `*` matches one path segment, and
/// a pattern without a glob is an exact path.
fn cargo_member_matches(relative: &str, pattern: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    if !pattern.contains('*') {
        return relative == pattern;
    }
    let relative_parts: Vec<&str> = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let pattern_parts: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    if relative_parts.len() != pattern_parts.len() {
        return false;
    }
    relative_parts
        .iter()
        .zip(pattern_parts.iter())
        .all(|(path_part, pattern_part)| *pattern_part == "*" || path_part == pattern_part)
}

/// Nearest ancestor of `project_dir`, bounded by `workspace_root`, whose
/// Cargo.toml has a `[workspace]` table. A crate with no such ancestor uses
/// its own directory, which is where Cargo writes `target/`.
fn cargo_workspace_root(project_dir: &Path, workspace_root: &Path) -> PathBuf {
    let mut current = project_dir.to_path_buf();
    loop {
        if cargo_toml_is_workspace(&current.join("Cargo.toml")) {
            return current;
        }
        if current == workspace_root {
            break;
        }
        match current.parent() {
            Some(parent) if parent.starts_with(workspace_root) => current = parent.to_path_buf(),
            _ => break,
        }
    }
    project_dir.to_path_buf()
}

fn cargo_toml_is_workspace(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<Value>(&content) else {
        return false;
    };
    value.get("workspace").is_some()
}

/// Path relative to the Aster workspace root, with `/` separators.
/// `~` paths are not passed here. A path equal to the workspace root is `""`.
fn workspace_relative(path: &Path, workspace_root: &Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_cargo_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
name = "my-crate"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let metadata = plugin.parse_project(tmp.path(), &cargo_toml).unwrap();

        assert_eq!(metadata.name, "my-crate");
        assert_eq!(metadata.version, Some("0.1.0".to_string()));
    }

    #[test]
    fn test_parse_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
shared = { path = "../shared" }
external = "1.0"

[dev-dependencies]
test-utils = { path = "../test-utils", features = ["mock"] }
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let deps = plugin.parse_dependencies(&cargo_toml).unwrap();

        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"shared"));
        assert!(names.contains(&"test-utils"));
    }

    #[test]
    fn test_parse_no_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let deps = plugin.parse_dependencies(&cargo_toml).unwrap();

        assert!(deps.is_empty());
    }

    #[test]
    fn test_workspace_root_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[workspace]
members = ["crates/*"]
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let result = plugin.parse_project(tmp.path(), &cargo_toml);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No [package] section"));
    }

    #[test]
    fn test_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
version = "0.1.0"
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let result = plugin.parse_project(tmp.path(), &cargo_toml);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing 'name'"));
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
    fn test_detect_targets_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
name = "my-crate"
version = "0.1.0"
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let ctx = make_context(&cargo_toml, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Check all expected targets exist
        assert!(targets.contains_key("deps"));
        assert!(targets.contains_key("build"));
        assert!(targets.contains_key("check"));
        assert!(targets.contains_key("test"));
        assert!(targets.contains_key("lint"));
        assert!(targets.contains_key("format"));

        // Check commands
        assert_eq!(targets.get("build").unwrap().command, "cargo build");
        assert_eq!(targets.get("test").unwrap().command, "cargo test");
        assert_eq!(targets.get("lint").unwrap().command, "cargo clippy");
        assert_eq!(targets.get("format").unwrap().command, "cargo fmt");

        // Check deps has no dependencies
        assert!(targets.get("deps").unwrap().depends_on.is_empty());

        // Check other targets depend on deps
        assert!(targets
            .get("build")
            .unwrap()
            .depends_on
            .contains(&"//self:deps".to_string()));
        assert!(targets
            .get("test")
            .unwrap()
            .depends_on
            .contains(&"//self:deps".to_string()));
    }

    #[test]
    fn test_detect_targets_with_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create workspace structure
        let project_dir = workspace_root.join("crates/app");
        std::fs::create_dir_all(&project_dir).unwrap();
        let cargo_toml = project_dir.join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
name = "app"
version = "0.1.0"
"#,
        )
        .unwrap();

        // Create dependency directory
        let lib_dir = workspace_root.join("crates/lib");
        std::fs::create_dir_all(&lib_dir).unwrap();

        // Dependencies with relative path
        let dependencies = vec![LocalDependency {
            name: "lib".to_string(),
            path: "../lib".into(),
        }];

        let plugin = RustPlugin;
        let ctx = make_context(&cargo_toml, &workspace_root, &dependencies);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // build should depend on deps and lib:build
        let build_deps = &targets.get("build").unwrap().depends_on;
        assert!(build_deps.contains(&"//self:deps".to_string()));
        assert!(build_deps.contains(&"//crates/lib:build".to_string()));
    }

    #[test]
    fn test_cache_inputs_basic() {
        let plugin = RustPlugin;
        let inputs = plugin.cache_inputs("build");

        assert!(inputs.source_globs.contains(&"src/**/*.rs".to_string()));
        assert!(inputs.config_files.contains(&"Cargo.toml".to_string()));
        assert!(inputs.config_files.contains(&"Cargo.lock".to_string()));
        assert!(inputs.env_vars.contains(&"RUSTFLAGS".to_string()));
    }

    #[test]
    fn test_cache_inputs_test_includes_test_files() {
        let plugin = RustPlugin;
        let inputs = plugin.cache_inputs("test");

        assert!(inputs.source_globs.contains(&"tests/**/*.rs".to_string()));
        assert!(inputs.source_globs.contains(&"benches/**/*.rs".to_string()));
    }

    #[test]
    fn test_detect_targets_has_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"
[package]
name = "my-crate"
version = "0.1.0"
"#,
        )
        .unwrap();

        let plugin = RustPlugin;
        let ctx = make_context(&cargo_toml, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert_eq!(clean.command, "cargo clean");
        assert!(clean.invalidates_cache);
    }

    #[test]
    fn workspace_member_cache_inputs_include_root_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n",
        )
        .unwrap();
        let member = tmp.path().join("crates/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let excluded = tmp.path().join("crates/skip");
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::write(
            excluded.join("Cargo.toml"),
            "[package]\nname = \"skip\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let nested = tmp.path().join("crates/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[package]\nname = \"nested\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let plugin = RustPlugin;
        assert!(plugin.should_skip(&tmp.path().join("Cargo.toml")));
        assert!(!plugin.should_skip(&member.join("Cargo.toml")));

        let build = plugin.workspace_cache_inputs(&member, tmp.path(), "build");
        assert_eq!(
            build,
            vec![
                "Cargo.toml".to_string(),
                "Cargo.lock".to_string(),
                "rust-toolchain.toml".to_string(),
                ".cargo/config.toml".to_string(),
            ]
        );
        assert_eq!(
            plugin.workspace_cache_inputs(&member, tmp.path(), "deps"),
            vec!["Cargo.toml".to_string(), "Cargo.lock".to_string()]
        );
        assert!(plugin
            .workspace_cache_inputs(&excluded, tmp.path(), "build")
            .is_empty());
        assert!(plugin
            .workspace_cache_inputs(&nested, tmp.path(), "build")
            .is_empty());
        assert!(plugin
            .workspace_cache_inputs(tmp.path(), tmp.path(), "build")
            .is_empty());
    }

    fn registry_paths() -> Vec<String> {
        vec![
            "~/.cargo/registry/index".to_string(),
            "~/.cargo/registry/cache".to_string(),
            "~/.cargo/git/db".to_string(),
        ]
    }

    #[test]
    fn test_native_caches_default_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("go.mod");
        std::fs::write(&config, "module example.com/app\n\ngo 1.22\n").unwrap();
        let ctx = make_context(&config, tmp.path(), &[]);

        let caches = super::super::GoPlugin.native_caches(&ctx);

        assert!(caches.is_empty());
    }

    #[test]
    fn test_native_caches_standalone_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let plugin = RustPlugin;
        let ctx = make_context(&cargo_toml, tmp.path(), &[]);

        let caches = plugin.native_caches(&ctx);

        assert_eq!(caches.len(), 2);
        assert_eq!(caches[0].id, "cargo-registry");
        assert_eq!(caches[0].paths, registry_paths());
        assert_eq!(caches[0].key_files, vec!["Cargo.lock".to_string()]);
        assert_eq!(caches[0].key_runtime, None);
        assert!(caches[0].shared);

        assert_eq!(caches[1].id, "cargo-target");
        assert_eq!(caches[1].paths, vec!["target".to_string()]);
        assert_eq!(caches[1].key_files, vec!["Cargo.lock".to_string()]);
        assert_eq!(caches[1].key_runtime.as_deref(), Some("rustc -vV"));
        assert!(!caches[1].shared);
    }

    #[test]
    fn test_native_caches_workspace_member_uses_workspace_target() {
        let tmp = tempfile::tempdir().unwrap();
        let aster_root = tmp.path();
        // Cargo workspace is nested under the Aster workspace, so the restored
        // target dir is relative to the checkout, not the member.
        let cargo_root = aster_root.join("services/api");
        std::fs::create_dir_all(&cargo_root).unwrap();
        std::fs::write(
            cargo_root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\"]\n",
        )
        .unwrap();
        let member = cargo_root.join("crates/app");
        std::fs::create_dir_all(&member).unwrap();
        let cargo_toml = member.join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let plugin = RustPlugin;
        let ctx = make_context(&cargo_toml, aster_root, &[]);

        let caches = plugin.native_caches(&ctx);

        assert_eq!(caches[0].id, "cargo-registry");
        assert_eq!(caches[0].paths, registry_paths());
        assert_eq!(
            caches[0].key_files,
            vec!["services/api/Cargo.lock".to_string()]
        );
        assert!(caches[0].shared);
        assert_eq!(caches[0].key_runtime, None);

        assert_eq!(caches[1].id, "cargo-target:services/api");
        assert_eq!(caches[1].paths, vec!["services/api/target".to_string()]);
        assert_eq!(
            caches[1].key_files,
            vec!["services/api/Cargo.lock".to_string()]
        );
        assert_eq!(caches[1].key_runtime.as_deref(), Some("rustc -vV"));
        assert!(!caches[1].shared);
    }
}
