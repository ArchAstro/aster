//! Rust language plugin for Cargo.toml parsing

use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use toml::Value;

use super::{LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetContext};

/// Rust plugin for discovering and parsing Cargo projects
pub struct RustPlugin;

impl LanguagePlugin for RustPlugin {
    fn name(&self) -> &str {
        "rust"
    }

    fn marker_files(&self) -> &[&str] {
        &["Cargo.toml"]
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let cargo_toml: Value = content
            .parse()
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

        let cargo_toml: Value = content
            .parse()
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
            },
        );

        // test target
        targets.insert(
            "test".to_string(),
            Target {
                command: "cargo test".to_string(),
                depends_on: base_deps.clone(),
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
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
            },
        );

        Ok(targets)
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        let mut inputs = super::CacheInputs {
            source_globs: vec!["src/**/*.rs".to_string(), "build.rs".to_string()],
            config_files: vec![
                "Cargo.toml".to_string(),
                "Cargo.lock".to_string(),
                "rust-toolchain.toml".to_string(),
                ".cargo/config.toml".to_string(),
            ],
            env_vars: vec![
                "RUSTFLAGS".to_string(),
                "CARGO_TARGET_DIR".to_string(),
                "CI".to_string(),
            ],
        };

        // Add test files for test target
        if target_name == "test" {
            inputs.source_globs.push("tests/**/*.rs".to_string());
            inputs.source_globs.push("benches/**/*.rs".to_string());
        }

        inputs
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
}
