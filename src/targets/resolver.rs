//! Target resolution with per-language detection
//!
//! Plugins detect available targets from native config files.
//! Custom targets from aster.toml override detected targets at the key level.

use std::collections::{HashMap, HashSet};

use crate::plugins::Target;

/// Standard target names
pub const TARGET_TEST: &str = "test";
pub const TARGET_BUILD: &str = "build";
pub const TARGET_LINT: &str = "lint";
pub const TARGET_DEPS: &str = "deps";

/// Resolves targets by merging detected targets with custom overrides
pub struct TargetResolver;

impl TargetResolver {
    /// Resolve targets by merging detected targets with custom overrides
    ///
    /// Custom targets from aster.toml override the command but preserve
    /// detected dependencies. New targets are added with empty depends_on.
    ///
    /// # Arguments
    /// * `detected_targets` - Targets detected from native config by the plugin
    /// * `custom_targets` - Custom targets from aster.toml (command strings only)
    /// * `project_address` - The project's address, used to resolve //self: references
    pub fn resolve(
        detected_targets: &HashMap<String, Target>,
        custom_targets: &HashMap<String, String>,
        project_address: &str,
    ) -> HashMap<String, Target> {
        let mut targets = HashMap::new();

        // Start with detected targets, resolving //self: references
        for (name, target) in detected_targets {
            let resolved_deps: Vec<String> = target
                .depends_on
                .iter()
                .map(|dep| {
                    if let Some(target_name) = dep.strip_prefix("//self:") {
                        format!("{}:{}", project_address, target_name)
                    } else {
                        dep.clone()
                    }
                })
                .collect();

            targets.insert(
                name.clone(),
                Target {
                    command: target.command.clone(),
                    depends_on: resolved_deps,
                    capabilities: target.capabilities.clone(),
                },
            );
        }

        // Custom targets override command but preserve depends_on and capabilities (if target exists)
        for (name, command) in custom_targets {
            if let Some(existing) = targets.get(name) {
                // Override command, keep depends_on and capabilities
                targets.insert(
                    name.clone(),
                    Target {
                        command: command.clone(),
                        depends_on: existing.depends_on.clone(),
                        capabilities: existing.capabilities.clone(),
                    },
                );
            } else {
                // New target with empty depends_on and capabilities
                targets.insert(
                    name.clone(),
                    Target {
                        command: command.clone(),
                        depends_on: vec![],
                        capabilities: HashSet::new(),
                    },
                );
            }
        }

        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(command: &str, depends_on: Vec<&str>) -> Target {
        Target {
            command: command.to_string(),
            depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
            capabilities: HashSet::new(),
        }
    }

    #[test]
    fn test_detected_targets_only() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));
        detected.insert("build".to_string(), target("npm run build", vec!["//self:deps"]));

        let targets = TargetResolver::resolve(&detected, &HashMap::new(), "//apps/web");

        assert_eq!(targets.get(TARGET_TEST).map(|t| &t.command), Some(&"npm test".to_string()));
        assert_eq!(targets.get(TARGET_BUILD).map(|t| &t.command), Some(&"npm run build".to_string()));
        assert_eq!(targets.get(TARGET_LINT), None);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_self_reference_resolved() {
        let mut detected = HashMap::new();
        detected.insert("deps".to_string(), target("npm install", vec![]));
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));

        let targets = TargetResolver::resolve(&detected, &HashMap::new(), "//apps/web");

        // //self:deps should be resolved to //apps/web:deps
        let test_target = targets.get("test").unwrap();
        assert_eq!(test_target.depends_on, vec!["//apps/web:deps".to_string()]);
    }

    #[test]
    fn test_custom_override_preserves_depends_on() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));
        detected.insert("build".to_string(), target("npm run build", vec!["//self:deps"]));

        let mut custom = HashMap::new();
        custom.insert("test".to_string(), "npm run test:ci".to_string());

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        // test command is overridden but depends_on is preserved
        let test_target = targets.get(TARGET_TEST).unwrap();
        assert_eq!(test_target.command, "npm run test:ci");
        assert_eq!(test_target.depends_on, vec!["//apps/web:deps".to_string()]);

        // build keeps detected values
        assert_eq!(targets.get(TARGET_BUILD).map(|t| &t.command), Some(&"npm run build".to_string()));
    }

    #[test]
    fn test_custom_addition() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec![]));

        let mut custom = HashMap::new();
        custom.insert("deploy".to_string(), "npm run deploy".to_string());

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        // Detected target still there
        assert_eq!(targets.get(TARGET_TEST).map(|t| &t.command), Some(&"npm test".to_string()));
        // Custom target added with empty depends_on
        let deploy = targets.get("deploy").unwrap();
        assert_eq!(deploy.command, "npm run deploy");
        assert!(deploy.depends_on.is_empty());
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_empty_detected_with_custom() {
        let detected = HashMap::new();
        let mut custom = HashMap::new();
        custom.insert("test".to_string(), "make test".to_string());

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        assert_eq!(targets.len(), 1);
        let test_target = targets.get("test").unwrap();
        assert_eq!(test_target.command, "make test");
        assert!(test_target.depends_on.is_empty());
    }

    #[test]
    fn test_both_empty() {
        let targets = TargetResolver::resolve(&HashMap::new(), &HashMap::new(), "//apps/web");
        assert!(targets.is_empty());
    }

    #[test]
    fn test_custom_completely_overrides_detected() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("pytest", vec!["//self:deps"]));
        detected.insert("lint".to_string(), target("ruff check .", vec!["//self:deps"]));

        let mut custom = HashMap::new();
        custom.insert("test".to_string(), "python -m pytest --cov".to_string());
        custom.insert("lint".to_string(), "mypy .".to_string());
        custom.insert("typecheck".to_string(), "pyright .".to_string());

        let targets = TargetResolver::resolve(&detected, &custom, "//libs/ml");

        // Commands are overridden
        assert_eq!(targets.get("test").map(|t| &t.command), Some(&"python -m pytest --cov".to_string()));
        assert_eq!(targets.get("lint").map(|t| &t.command), Some(&"mypy .".to_string()));
        // But depends_on is preserved for existing targets
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//libs/ml:deps".to_string()]);
        assert_eq!(targets.get("lint").unwrap().depends_on, vec!["//libs/ml:deps".to_string()]);
        // New target has empty depends_on
        assert!(targets.get("typecheck").unwrap().depends_on.is_empty());
        assert_eq!(targets.len(), 3);
    }
}
