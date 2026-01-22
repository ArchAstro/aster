//! Elixir language plugin for mix.exs parsing

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{LanguagePlugin, LocalDependency, ProjectMetadata};

/// Regex to extract app name from `app: :name` in mix.exs project definition
static APP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"app:\s*:(\w+)").expect("Invalid APP_REGEX")
});

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
                path: PathBuf::from(format!("../{}", name)),
            });
        }

        Ok(deps)
    }
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
}
