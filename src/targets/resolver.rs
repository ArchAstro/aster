//! Target resolution with per-language defaults
//!
//! Maps standard target names (test, build, lint) to native commands per language.
//! Custom targets from aster.toml override defaults at the key level.

use std::collections::HashMap;

/// Standard target names
pub const TARGET_TEST: &str = "test";
pub const TARGET_BUILD: &str = "build";
pub const TARGET_LINT: &str = "lint";

/// Resolves targets for a given plugin, merging defaults with custom overrides
pub struct TargetResolver;

impl TargetResolver {
    /// Resolve targets for a plugin, merging defaults with custom overrides
    ///
    /// Returns a map of target names to commands. Custom targets override
    /// defaults at the key level (not wholesale replacement).
    pub fn resolve(
        plugin_name: &str,
        custom_targets: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        let mut targets = defaults_for_plugin(plugin_name);
        // Custom targets override defaults (merge at key level)
        for (name, command) in custom_targets {
            targets.insert(name.clone(), command.clone());
        }
        targets
    }
}

/// Get default target commands for a plugin
fn defaults_for_plugin(plugin_name: &str) -> HashMap<String, String> {
    match plugin_name {
        "nodejs" => {
            let mut m = HashMap::new();
            m.insert(TARGET_TEST.to_string(), "npm test".to_string());
            m.insert(TARGET_BUILD.to_string(), "npm run build".to_string());
            m.insert(TARGET_LINT.to_string(), "npm run lint".to_string());
            m
        }
        "elixir" => {
            let mut m = HashMap::new();
            m.insert(TARGET_TEST.to_string(), "mix test".to_string());
            m.insert(TARGET_BUILD.to_string(), "mix compile".to_string());
            m.insert(TARGET_LINT.to_string(), "mix credo".to_string());
            m
        }
        "python" => {
            let mut m = HashMap::new();
            m.insert(TARGET_TEST.to_string(), "pytest".to_string());
            m.insert(TARGET_BUILD.to_string(), "python -m build".to_string());
            m.insert(TARGET_LINT.to_string(), "ruff check .".to_string());
            m
        }
        _ => HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nodejs_defaults() {
        let targets = TargetResolver::resolve("nodejs", &HashMap::new());

        assert_eq!(targets.get(TARGET_TEST), Some(&"npm test".to_string()));
        assert_eq!(targets.get(TARGET_BUILD), Some(&"npm run build".to_string()));
        assert_eq!(targets.get(TARGET_LINT), Some(&"npm run lint".to_string()));
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn test_elixir_defaults() {
        let targets = TargetResolver::resolve("elixir", &HashMap::new());

        assert_eq!(targets.get(TARGET_TEST), Some(&"mix test".to_string()));
        assert_eq!(targets.get(TARGET_BUILD), Some(&"mix compile".to_string()));
        assert_eq!(targets.get(TARGET_LINT), Some(&"mix credo".to_string()));
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn test_python_defaults() {
        let targets = TargetResolver::resolve("python", &HashMap::new());

        assert_eq!(targets.get(TARGET_TEST), Some(&"pytest".to_string()));
        assert_eq!(targets.get(TARGET_BUILD), Some(&"python -m build".to_string()));
        assert_eq!(targets.get(TARGET_LINT), Some(&"ruff check .".to_string()));
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn test_custom_override() {
        let mut custom = HashMap::new();
        custom.insert("test".to_string(), "npm run test:ci".to_string());

        let targets = TargetResolver::resolve("nodejs", &custom);

        // test is overridden
        assert_eq!(targets.get(TARGET_TEST), Some(&"npm run test:ci".to_string()));
        // build and lint keep defaults
        assert_eq!(targets.get(TARGET_BUILD), Some(&"npm run build".to_string()));
        assert_eq!(targets.get(TARGET_LINT), Some(&"npm run lint".to_string()));
    }

    #[test]
    fn test_custom_addition() {
        let mut custom = HashMap::new();
        custom.insert("deploy".to_string(), "npm run deploy".to_string());

        let targets = TargetResolver::resolve("nodejs", &custom);

        // Defaults are still there
        assert_eq!(targets.get(TARGET_TEST), Some(&"npm test".to_string()));
        assert_eq!(targets.get(TARGET_BUILD), Some(&"npm run build".to_string()));
        assert_eq!(targets.get(TARGET_LINT), Some(&"npm run lint".to_string()));
        // Custom target is added
        assert_eq!(targets.get("deploy"), Some(&"npm run deploy".to_string()));
        assert_eq!(targets.len(), 4);
    }

    #[test]
    fn test_unknown_plugin() {
        let targets = TargetResolver::resolve("unknown", &HashMap::new());

        // Unknown plugin has no defaults
        assert!(targets.is_empty());
    }

    #[test]
    fn test_unknown_plugin_with_custom() {
        let mut custom = HashMap::new();
        custom.insert("test".to_string(), "make test".to_string());

        let targets = TargetResolver::resolve("unknown", &custom);

        // Only custom targets are present
        assert_eq!(targets.len(), 1);
        assert_eq!(targets.get("test"), Some(&"make test".to_string()));
    }
}
