//! Elixir language plugin for mix.exs parsing

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{
    LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetCapability, TargetContext,
};

/// Regex to extract app name from `app: :name` in mix.exs project definition
static APP_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"app:\s*:(\w+)").expect("Invalid APP_REGEX"));

/// Regex to detect umbrella projects via `apps_path:` in mix.exs
/// Umbrella roots define apps_path instead of app - they aren't buildable projects themselves
static APPS_PATH_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"apps_path:\s*").expect("Invalid APPS_PATH_REGEX"));

/// Regex to match path dependencies: `{:name, path: "../path"}`
/// Also handles optional in_umbrella: true at the end
static PATH_DEP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{:(\w+),\s*path:\s*"([^"]+)"(?:\s*,\s*in_umbrella:\s*(?:true|false))?\s*\}"#)
        .expect("Invalid PATH_DEP_REGEX")
});

/// Regex to match in_umbrella-only dependencies: `{:name, in_umbrella: true}`
/// These don't have an explicit path and resolve to `../name`
static IN_UMBRELLA_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{:(\w+),\s*in_umbrella:\s*true\s*\}"#).expect("Invalid IN_UMBRELLA_REGEX")
});

/// Elixir plugin for discovering and parsing mix.exs projects
pub struct ElixirPlugin;

impl LanguagePlugin for ElixirPlugin {
    fn name(&self) -> &str {
        "elixir"
    }

    fn marker_files(&self) -> &[&str] {
        &["mix.exs"]
    }

    fn should_skip(&self, _config_path: &Path) -> bool {
        // Don't skip umbrella roots - we handle them specially in parse_project() and detect_targets()
        // Umbrella roots get only a deps target, children get transformed commands
        false
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        // Check if this is an umbrella root (has apps_path: but no app:)
        if APPS_PATH_REGEX.is_match(&content) {
            // Use a synthetic name based on directory name
            let dir_name = config_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("umbrella");
            return Ok(ProjectMetadata {
                name: format!("{dir_name}_umbrella"),
                version: None,
            });
        }

        // Regular app - extract app name
        let name = APP_REGEX
            .captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| {
                anyhow!(
                    "Could not find 'app: :name' in {}. Elixir projects must define an app name.",
                    config_path.display()
                )
            })?;

        Ok(ProjectMetadata {
            name,
            version: None, // Could extract version if needed
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        // Normalize whitespace to handle multiline dependencies
        let normalized = normalize_whitespace(&content);

        let mut deps = Vec::new();

        // Extract path: dependencies (with or without in_umbrella flag)
        for caps in PATH_DEP_REGEX.captures_iter(&normalized) {
            let name = caps.get(1).unwrap().as_str();
            let path = caps.get(2).unwrap().as_str();
            deps.push(LocalDependency {
                name: name.to_string(),
                path: PathBuf::from(path),
            });
        }

        // Extract in_umbrella: true dependencies
        // Use special prefix to mark these for post-discovery resolution
        // (app names often differ from directory names in umbrella projects)
        for caps in IN_UMBRELLA_REGEX.captures_iter(&normalized) {
            let name = caps.get(1).unwrap().as_str();
            deps.push(LocalDependency {
                name: name.to_string(),
                path: PathBuf::from(format!("in_umbrella:{name}")),
            });
        }

        Ok(deps)
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let content = std::fs::read_to_string(ctx.config_path)
            .with_context(|| format!("Failed to read {}", ctx.config_path.display()))?;

        // Check if this is an umbrella root (has apps_path:)
        if APPS_PATH_REGEX.is_match(&content) {
            // Umbrella root: only deps target
            let mut targets = HashMap::new();
            targets.insert(
                "deps".to_string(),
                Target {
                    command: "mix deps.get".to_string(),
                    depends_on: vec![],
                    capabilities: HashSet::new(),
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                    exclusive_resources: vec!["hex_registry".into()],
                },
            );
            return Ok(targets);
        }

        // Check if this is an umbrella child
        let umbrella_root = find_umbrella_root(ctx.project_dir);

        if let Some(ref umbrella_path) = umbrella_root {
            // Umbrella child: use mix do --app and run from umbrella root
            return detect_umbrella_child_targets(ctx, &content, umbrella_path);
        }

        // Regular standalone Elixir project
        detect_standalone_targets(ctx, &content)
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

        // Filter to Elixir test files only (*_test.exs in test/ directory)
        let test_files: Vec<&PathBuf> = files
            .iter()
            .filter(|f| {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let path_str = f.to_string_lossy();
                name.ends_with("_test.exs") || path_str.contains("test/")
            })
            .collect();

        if test_files.is_empty() {
            // No test files in the change set - run full test suite
            return None;
        }

        // mix test file1_test.exs file2_test.exs (mix test accepts file paths directly)
        let file_args: Vec<String> = test_files
            .iter()
            .map(|f| crate::executor::quote_command_argument(&f.to_string_lossy()))
            .collect();

        Some(format!("{} {}", command, file_args.join(" ")))
    }

    fn with_warnings_as_errors(&self, target_name: &str, command: &str) -> Option<String> {
        // Both mix compile and mix test support --warnings-as-errors
        match target_name {
            "build" | "test" => Some(format!("{command} --warnings-as-errors")),
            _ => None,
        }
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        // deps target only cares about dependency specification files,
        // not source code — source changes shouldn't re-fetch dependencies
        if target_name == "deps" {
            return super::CacheInputs {
                source_globs: vec![],
                config_files: vec!["mix.exs".to_string(), "mix.lock".to_string()],
                env_vars: vec!["MIX_ENV".to_string()],
            };
        }

        let mut inputs = super::CacheInputs {
            source_globs: vec!["lib/**/*.ex".to_string(), "lib/**/*.exs".to_string()],
            config_files: vec![
                "mix.exs".to_string(),
                "mix.lock".to_string(),
                "config/*.exs".to_string(),
            ],
            env_vars: vec!["MIX_ENV".to_string(), "CI".to_string()],
        };

        if target_name == "test" {
            inputs.source_globs.push("test/**/*.exs".to_string());
        }

        inputs
    }

    fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
        Some(Target {
            command: "mix clean".to_string(),
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

/// Normalize whitespace in mix.exs content to handle multiline dependency declarations
/// This replaces sequences of whitespace (including newlines) with single spaces
fn normalize_whitespace(content: &str) -> String {
    static WS_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s+").expect("Invalid whitespace regex"));
    WS_REGEX.replace_all(content, " ").to_string()
}

/// Check if a project is a child of an umbrella project
/// Returns Some(umbrella_root_path) if this is an umbrella child, None otherwise
///
/// Checks both parent and grandparent directories to handle both:
/// - apps_path: "." (children directly in umbrella root)
/// - apps_path: "apps" (children in apps/ subdirectory)
fn find_umbrella_root(project_dir: &Path) -> Option<PathBuf> {
    // Check parent directory for umbrella mix.exs (apps_path: ".")
    let parent = project_dir.parent()?;
    let parent_mix = parent.join("mix.exs");
    if parent_mix.exists() {
        let content = std::fs::read_to_string(&parent_mix).ok()?;
        if APPS_PATH_REGEX.is_match(&content) {
            return Some(parent.to_path_buf());
        }
    }

    // Check grandparent directory for umbrella mix.exs (apps_path: "apps")
    let grandparent = parent.parent()?;
    let grandparent_mix = grandparent.join("mix.exs");
    if grandparent_mix.exists() {
        let content = std::fs::read_to_string(&grandparent_mix).ok()?;
        if APPS_PATH_REGEX.is_match(&content) {
            return Some(grandparent.to_path_buf());
        }
    }

    None
}

/// Detect targets for umbrella child apps
/// Commands are transformed to use `mix do --app <name>` and run from umbrella root
fn detect_umbrella_child_targets(
    ctx: &TargetContext,
    content: &str,
    umbrella_root: &Path,
) -> Result<HashMap<String, Target>> {
    let mut targets = HashMap::new();

    // Extract app name for mix do --app
    let app_name = APP_REGEX
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .ok_or_else(|| {
            anyhow!(
                "Could not find 'app: :name' in {}",
                ctx.config_path.display()
            )
        })?;

    // Compute umbrella deps address (e.g., "//src/elixir:deps")
    let umbrella_rel = umbrella_root
        .strip_prefix(ctx.workspace_root)
        .unwrap_or(umbrella_root);
    let umbrella_deps = format!("//{}:deps", umbrella_rel.display());

    // Base deps: umbrella deps + cross-language deps from aster.toml
    // Note: We intentionally do NOT add Elixir-to-Elixir :build dependencies here.
    // Mix handles internal dependency compilation automatically - adding explicit
    // target dependencies would cause double builds. Project-level dependencies
    // are still tracked via parse_dependencies() for affected analysis.
    //
    // However, cross-language dependencies (from aster.toml, with // prefix) ARE added
    // as target dependencies since Mix doesn't manage those.
    let mut base_deps = vec![umbrella_deps.clone()];
    for dep in ctx.dependencies {
        let path_str = dep.path.to_string_lossy();
        // Only add cross-language deps (those with // prefix from aster.toml)
        // Skip Elixir deps (relative paths or in_umbrella: prefix from mix.exs)
        if path_str.starts_with("//") {
            // Ensure we're referencing the :build target
            if path_str.contains(':') {
                base_deps.push(path_str.to_string());
            } else {
                base_deps.push(format!("{path_str}:build"));
            }
        }
    }

    // build target
    let mut build_caps = HashSet::new();
    build_caps.insert(TargetCapability::WarningsAsErrors);
    targets.insert(
        "build".to_string(),
        Target {
            command: format!("mix do --app {app_name} compile"),
            depends_on: base_deps.clone(),
            capabilities: build_caps,
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: Some(umbrella_root.to_path_buf()),
            exclusive_resources: vec![],
        },
    );

    // test target
    let mut test_caps = HashSet::new();
    test_caps.insert(TargetCapability::FilesList);
    test_caps.insert(TargetCapability::WarningsAsErrors);
    targets.insert(
        "test".to_string(),
        Target {
            command: format!("MIX_ENV=test mix do --app {app_name} test"),
            depends_on: base_deps.clone(),
            capabilities: test_caps,
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: Some(umbrella_root.to_path_buf()),
            exclusive_resources: vec![],
        },
    );

    // format target (depends on build)
    targets.insert(
        "format".to_string(),
        Target {
            command: format!("mix do --app {app_name} format"),
            depends_on: vec![umbrella_deps.clone(), "//self:build".to_string()],
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: Some(umbrella_root.to_path_buf()),
            exclusive_resources: vec![],
        },
    );

    // lint target (if credo present)
    if content.contains(":credo") {
        targets.insert(
            "lint".to_string(),
            Target {
                command: format!("mix do --app {app_name} credo"),
                depends_on: base_deps,
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: Some(umbrella_root.to_path_buf()),
                exclusive_resources: vec![],
            },
        );
    }

    // clean target (no deps, invalidates cache)
    targets.insert(
        "clean".to_string(),
        Target {
            command: format!("mix do --app {app_name} clean"),
            depends_on: vec![],
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: true,
            working_dir: Some(umbrella_root.to_path_buf()),
            exclusive_resources: vec![],
        },
    );

    Ok(targets)
}

/// Detect targets for standalone (non-umbrella) Elixir projects
fn detect_standalone_targets(
    ctx: &TargetContext,
    content: &str,
) -> Result<HashMap<String, Target>> {
    let mut targets = HashMap::new();

    // Always add deps target for mix deps.get (no cross-project dependencies)
    targets.insert(
        "deps".to_string(),
        Target {
            command: "mix deps.get".to_string(),
            depends_on: vec![],
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
            exclusive_resources: vec!["hex_registry".into()],
        },
    );

    // Base deps: our own deps + cross-language deps from aster.toml
    // Note: We intentionally do NOT add Elixir-to-Elixir :build dependencies here.
    // Mix handles internal dependency compilation automatically - adding explicit
    // target dependencies would cause double builds. Project-level dependencies
    // are still tracked via parse_dependencies() for affected analysis.
    //
    // However, cross-language dependencies (from aster.toml, with // prefix) ARE added
    // as target dependencies since Mix doesn't manage those.
    let mut base_deps = vec!["//self:deps".to_string()];
    for dep in ctx.dependencies {
        let path_str = dep.path.to_string_lossy();
        // Only add cross-language deps (those with // prefix from aster.toml)
        // Skip Elixir deps (relative paths or in_umbrella: prefix from mix.exs)
        if path_str.starts_with("//") {
            // Ensure we're referencing the :build target
            if path_str.contains(':') {
                base_deps.push(path_str.to_string());
            } else {
                base_deps.push(format!("{path_str}:build"));
            }
        }
    }

    // mix test and mix compile are always available for Elixir projects
    let mut test_caps = HashSet::new();
    test_caps.insert(TargetCapability::FilesList);
    test_caps.insert(TargetCapability::WarningsAsErrors);
    targets.insert(
        "test".to_string(),
        Target {
            command: "mix test".to_string(),
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

    let mut build_caps = HashSet::new();
    build_caps.insert(TargetCapability::WarningsAsErrors);
    targets.insert(
        "build".to_string(),
        Target {
            command: "mix compile".to_string(),
            depends_on: base_deps.clone(),
            capabilities: build_caps,
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
            exclusive_resources: vec![],
        },
    );

    // Only add lint if credo is a dependency
    if content.contains(":credo") {
        targets.insert(
            "lint".to_string(),
            Target {
                command: "mix credo".to_string(),
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

    // format target: only if .formatter.exs exists
    if ctx.project_dir.join(".formatter.exs").exists() {
        targets.insert(
            "format".to_string(),
            Target {
                command: "mix format".to_string(),
                depends_on: vec!["//self:deps".to_string(), "//self:build".to_string()],
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
    targets.insert(
        "clean".to_string(),
        Target {
            command: "mix clean".to_string(),
            depends_on: vec![],
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: true,
            working_dir: None,
            exclusive_resources: vec![],
        },
    );

    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_mix_exs() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [
      app: :my_app,
      version: "0.1.0",
      elixir: "~> 1.14",
      deps: deps()
    ]
  end

  defp deps do
    []
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let metadata = plugin.parse_project(tmp.path(), &mix_exs).unwrap();

        assert_eq!(metadata.name, "my_app");
        assert_eq!(metadata.version, None);
    }

    #[test]
    fn test_parse_path_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [
      {:shared_lib, path: "../shared_lib"}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let deps = plugin.parse_dependencies(&mix_exs).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "shared_lib");
        assert_eq!(deps[0].path, PathBuf::from("../shared_lib"));
    }

    #[test]
    fn test_parse_multiple_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [
      {:core, path: "../core"},
      {:utils, path: "../../libs/utils"},
      {:jason, "~> 1.4"}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let deps = plugin.parse_dependencies(&mix_exs).unwrap();

        // Only path: dependencies, not version deps like {:jason, "~> 1.4"}
        assert_eq!(deps.len(), 2);

        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"core"));
        assert!(names.contains(&"utils"));
    }

    #[test]
    fn test_parse_umbrella_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [
      {:sibling_app, in_umbrella: true}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let deps = plugin.parse_dependencies(&mix_exs).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "sibling_app");
        // in_umbrella deps are marked for post-discovery resolution
        assert_eq!(deps[0].path, PathBuf::from("in_umbrella:sibling_app"));
    }

    #[test]
    fn test_parse_path_with_in_umbrella_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [
      {:sibling, path: "../sibling", in_umbrella: true}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let deps = plugin.parse_dependencies(&mix_exs).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "sibling");
        assert_eq!(deps[0].path, PathBuf::from("../sibling"));
    }

    #[test]
    fn test_parse_multiline_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [
      {:multiline_dep,
        path: "../multiline_dep",
        in_umbrella: true}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let deps = plugin.parse_dependencies(&mix_exs).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "multiline_dep");
        assert_eq!(deps[0].path, PathBuf::from("../multiline_dep"));
    }

    #[test]
    fn test_missing_app_name() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [
      version: "0.1.0"
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let result = plugin.parse_project(tmp.path(), &mix_exs);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Could not find 'app: :name'"));
    }

    #[test]
    fn test_should_not_skip_umbrella_root() {
        // Umbrella roots are no longer skipped - they get a deps target only
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
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

  defp deps do
    []
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        assert!(!plugin.should_skip(&mix_exs));
    }

    #[test]
    fn test_should_not_skip_umbrella_with_apps_dir() {
        // Umbrella roots are no longer skipped - they get a deps target only
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyUmbrella.MixProject do
  use Mix.Project

  def project do
    [
      apps_path: "apps",
      version: "0.1.0",
      deps: deps()
    ]
  end

  defp deps do
    []
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        assert!(!plugin.should_skip(&mix_exs));
    }

    #[test]
    fn test_should_not_skip_regular_project() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [
      app: :my_app,
      version: "0.1.0",
      elixir: "~> 1.14",
      deps: deps()
    ]
  end

  defp deps do
    []
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        assert!(!plugin.should_skip(&mix_exs));
    }

    #[test]
    fn test_should_not_skip_umbrella_child_app() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
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
    [
      {:archastro_config, in_umbrella: true},
      {:archastro_workflow, in_umbrella: true}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        assert!(!plugin.should_skip(&mix_exs));
    }

    #[test]
    fn test_no_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [
      {:jason, "~> 1.4"},
      {:phoenix, "~> 1.7"},
      {:ecto, "~> 3.10"}
    ]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let deps = plugin.parse_dependencies(&mix_exs).unwrap();

        assert!(deps.is_empty());
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
    fn test_detect_targets_with_credo() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [{:credo, "~> 1.7", only: [:dev, :test]}]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test, build, deps always available
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"mix test".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"mix compile".to_string())
        );
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"mix deps.get".to_string())
        );
        // lint available because credo is a dependency
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"mix credo".to_string())
        );

        // Check dependencies
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//self:deps"]);
        assert_eq!(
            targets.get("build").unwrap().depends_on,
            vec!["//self:deps"]
        );
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
    }

    #[test]
    fn test_detect_targets_with_formatter() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        // Create .formatter.exs
        std::fs::write(
            tmp.path().join(".formatter.exs"),
            r#"[inputs: ["{config,lib,test}/**/*.{ex,exs}"]]"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format available because .formatter.exs exists
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"mix format".to_string())
        );

        // Check dependencies
        let format_deps = &targets.get("format").unwrap().depends_on;
        assert!(format_deps.contains(&"//self:deps".to_string()));
        assert!(format_deps.contains(&"//self:build".to_string()));
        assert_eq!(format_deps.len(), 2);
    }

    #[test]
    fn test_detect_targets_without_formatter() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        // No .formatter.exs

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format NOT available (no .formatter.exs)
        assert_eq!(targets.get("format"), None);
    }

    #[test]
    fn test_detect_targets_without_credo() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end

  defp deps do
    [{:jason, "~> 1.4"}]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test, build, deps always available
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"mix test".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"mix compile".to_string())
        );
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"mix deps.get".to_string())
        );
        // lint NOT available (no credo)
        assert_eq!(targets.get("lint"), None);
    }

    #[test]
    fn test_with_files_list_filters_test_files() {
        let plugin = ElixirPlugin;
        let files = vec![
            PathBuf::from("lib/my_app.ex"),
            PathBuf::from("test/my_app_test.exs"),
            PathBuf::from("test/support/helper.ex"),
            PathBuf::from("mix.exs"),
        ];

        let result = plugin.with_files_list("test", "mix test", &files);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.starts_with("mix test "));
        assert!(cmd.contains("my_app_test.exs"));
        assert!(cmd.contains("test/support/helper.ex")); // In test/ directory
        assert!(!cmd.contains("lib/my_app.ex"));
        assert!(!cmd.contains("mix.exs"));
    }

    #[test]
    fn test_with_files_list_returns_none_for_non_test_target() {
        let plugin = ElixirPlugin;
        let files = vec![PathBuf::from("test/my_app_test.exs")];

        let result = plugin.with_files_list("build", "mix compile", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_returns_none_when_no_test_files() {
        let plugin = ElixirPlugin;
        let files = vec![
            PathBuf::from("lib/my_app.ex"),
            PathBuf::from("lib/my_app/utils.ex"),
            PathBuf::from("mix.exs"),
        ];

        let result = plugin.with_files_list("test", "mix test", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_test_target_has_files_list_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project
  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));

        let deps_target = targets.get("deps").unwrap();
        assert!(!deps_target
            .capabilities
            .contains(&TargetCapability::FilesList));

        let build_target = targets.get("build").unwrap();
        assert!(!build_target
            .capabilities
            .contains(&TargetCapability::FilesList));
    }

    #[test]
    fn test_build_and_test_targets_have_warnings_as_errors_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project
  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::WarningsAsErrors));

        let build_target = targets.get("build").unwrap();
        assert!(build_target
            .capabilities
            .contains(&TargetCapability::WarningsAsErrors));

        let deps_target = targets.get("deps").unwrap();
        assert!(!deps_target
            .capabilities
            .contains(&TargetCapability::WarningsAsErrors));
    }

    #[test]
    fn test_with_warnings_as_errors_for_build() {
        let plugin = ElixirPlugin;
        let result = plugin.with_warnings_as_errors("build", "mix compile");
        assert_eq!(result, Some("mix compile --warnings-as-errors".to_string()));
    }

    #[test]
    fn test_with_warnings_as_errors_for_test() {
        let plugin = ElixirPlugin;
        let result = plugin.with_warnings_as_errors("test", "mix test");
        assert_eq!(result, Some("mix test --warnings-as-errors".to_string()));
    }

    #[test]
    fn test_with_warnings_as_errors_returns_none_for_unsupported_target() {
        let plugin = ElixirPlugin;
        let result = plugin.with_warnings_as_errors("deps", "mix deps.get");
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_targets_has_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project
  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert_eq!(clean.command, "mix clean");
        assert!(clean.depends_on.is_empty());
        assert!(clean.invalidates_cache);
    }

    #[test]
    fn test_elixir_deps_not_added_to_target_depends_on() {
        // Elixir-to-Elixir dependencies (from mix.exs) should NOT be added
        // to target depends_on because Mix handles internal compilation
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        // Simulate Elixir deps from mix.exs (relative paths)
        let elixir_deps = vec![
            LocalDependency {
                name: "core".to_string(),
                path: PathBuf::from("../core"),
            },
            LocalDependency {
                name: "shared".to_string(),
                path: PathBuf::from("in_umbrella:shared"),
            },
        ];

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &elixir_deps);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Should only have //self:deps, NOT the Elixir dependencies
        assert_eq!(
            targets.get("build").unwrap().depends_on,
            vec!["//self:deps"],
            "Elixir-to-Elixir deps should not be in target depends_on"
        );
        assert_eq!(
            targets.get("test").unwrap().depends_on,
            vec!["//self:deps"],
            "Elixir-to-Elixir deps should not be in target depends_on"
        );
    }

    #[test]
    fn test_cross_language_deps_added_to_target_depends_on() {
        // Cross-language dependencies (from aster.toml, with // prefix)
        // SHOULD be added to target depends_on since Mix doesn't manage them
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        // Simulate cross-language deps from aster.toml (// prefix)
        let cross_lang_deps = vec![
            LocalDependency {
                name: "//libs/shared:build".to_string(),
                path: PathBuf::from("//libs/shared:build"),
            },
            LocalDependency {
                name: "//libs/utils".to_string(),
                path: PathBuf::from("//libs/utils"),
            },
        ];

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &cross_lang_deps);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Should have //self:deps AND the cross-language dependencies
        let build_deps = &targets.get("build").unwrap().depends_on;
        assert!(
            build_deps.contains(&"//self:deps".to_string()),
            "Should have //self:deps"
        );
        assert!(
            build_deps.contains(&"//libs/shared:build".to_string()),
            "Should have cross-language dep with explicit target"
        );
        assert!(
            build_deps.contains(&"//libs/utils:build".to_string()),
            "Should add :build suffix to cross-language dep without target"
        );
    }

    #[test]
    fn test_mixed_deps_only_cross_language_in_target_depends_on() {
        // When both Elixir and cross-language deps exist, only cross-language
        // should be in target depends_on
        let tmp = tempfile::tempdir().unwrap();
        let mix_exs = tmp.path().join("mix.exs");
        std::fs::write(
            &mix_exs,
            r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app]
  end
end
"#,
        )
        .unwrap();

        // Mix of Elixir deps and cross-language deps
        let mixed_deps = vec![
            // Elixir dep (relative path) - should NOT be added
            LocalDependency {
                name: "core".to_string(),
                path: PathBuf::from("../core"),
            },
            // Cross-language dep (// prefix) - SHOULD be added
            LocalDependency {
                name: "//libs/nodejs-lib:build".to_string(),
                path: PathBuf::from("//libs/nodejs-lib:build"),
            },
            // Elixir umbrella dep - should NOT be added
            LocalDependency {
                name: "shared".to_string(),
                path: PathBuf::from("in_umbrella:shared"),
            },
        ];

        let plugin = ElixirPlugin;
        let ctx = make_context(&mix_exs, tmp.path(), &mixed_deps);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let build_deps = &targets.get("build").unwrap().depends_on;

        // Should have exactly 2 deps: //self:deps and the cross-language dep
        assert_eq!(build_deps.len(), 2, "Should have exactly 2 dependencies");
        assert!(build_deps.contains(&"//self:deps".to_string()));
        assert!(build_deps.contains(&"//libs/nodejs-lib:build".to_string()));

        // Should NOT contain Elixir deps
        assert!(
            !build_deps.iter().any(|d| d.contains("core")),
            "Should not contain Elixir dep 'core'"
        );
        assert!(
            !build_deps.iter().any(|d| d.contains("shared")),
            "Should not contain Elixir umbrella dep 'shared'"
        );
    }
}
