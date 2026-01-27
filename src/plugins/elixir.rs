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

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

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

        // Extract in_umbrella: true dependencies (implicit path: "../name")
        for caps in IN_UMBRELLA_REGEX.captures_iter(&normalized) {
            let name = caps.get(1).unwrap().as_str();
            // in_umbrella: true implies the sibling is in ../name relative to current app
            deps.push(LocalDependency {
                name: name.to_string(),
                path: PathBuf::from(format!("../{name}")),
            });
        }

        Ok(deps)
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let content = std::fs::read_to_string(ctx.config_path)
            .with_context(|| format!("Failed to read {}", ctx.config_path.display()))?;

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
                },
            );
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
            .map(|f| f.to_string_lossy().to_string())
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

/// Normalize whitespace in mix.exs content to handle multiline dependency declarations
/// This replaces sequences of whitespace (including newlines) with single spaces
fn normalize_whitespace(content: &str) -> String {
    static WS_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s+").expect("Invalid whitespace regex"));
    WS_REGEX.replace_all(content, " ").to_string()
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
        assert_eq!(deps[0].path, PathBuf::from("../sibling_app"));
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
}
