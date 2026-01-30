//! Target resolution with per-language detection
//!
//! Plugins detect available targets from native config files.
//! Custom targets from aster.toml override detected targets at the key level.

use std::collections::{HashMap, HashSet};

use crate::config::TargetConfig;
use crate::plugins::{Target, TargetCapability};

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
    /// Custom targets from aster.toml override detected targets. For simple format
    /// (just a command string), the command is overridden but depends_on/capabilities
    /// are preserved from detected. For rich format, all specified fields are used.
    ///
    /// # Arguments
    /// * `detected_targets` - Targets detected from native config by the plugin
    /// * `custom_targets` - Custom targets from aster.toml (simple or rich format)
    /// * `project_address` - The project's address, used to resolve //self: references
    pub fn resolve(
        detected_targets: &HashMap<String, Target>,
        custom_targets: &HashMap<String, TargetConfig>,
        project_address: &str,
    ) -> HashMap<String, Target> {
        let mut targets = HashMap::new();

        // Start with detected targets, resolving //self: references
        for (name, target) in detected_targets {
            let resolved_deps = Self::resolve_self_references(&target.depends_on, project_address);

            targets.insert(
                name.clone(),
                Target {
                    command: target.command.clone(),
                    depends_on: resolved_deps,
                    capabilities: target.capabilities.clone(),
                    files_glob: target.files_glob.clone(),
                    stream: target.stream,
                    cache: target.cache.clone(),
                    invalidates_cache: target.invalidates_cache,
                    working_dir: target.working_dir.clone(),
                },
            );
        }

        // First pass: process Simple and Rich custom targets
        for (name, config) in custom_targets {
            match config {
                TargetConfig::Simple(command) => {
                    let existing = targets.get(name);
                    // Simple format: override command, preserve existing depends_on/capabilities
                    if let Some(existing) = existing {
                        targets.insert(
                            name.clone(),
                            Target {
                                command: command.clone(),
                                depends_on: existing.depends_on.clone(),
                                capabilities: existing.capabilities.clone(),
                                files_glob: existing.files_glob.clone(),
                                stream: existing.stream,
                                cache: existing.cache.clone(),
                                invalidates_cache: existing.invalidates_cache,
                                working_dir: existing.working_dir.clone(),
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
                                files_glob: None,
                                stream: false,
                                cache: None,
                                invalidates_cache: false,
                                working_dir: None,
                            },
                        );
                    }
                }
                TargetConfig::Rich(rich) => {
                    let existing = targets.get(name);
                    // Rich format: use all specified fields, fall back to existing for unspecified
                    let depends_on = if rich.depends_on.is_empty() {
                        existing.map(|e| e.depends_on.clone()).unwrap_or_default()
                    } else {
                        Self::resolve_self_references(&rich.depends_on, project_address)
                    };

                    let capabilities = if rich.capabilities.is_empty() {
                        existing.map(|e| e.capabilities.clone()).unwrap_or_default()
                    } else {
                        Self::parse_capabilities(&rich.capabilities)
                    };

                    let files_glob = rich
                        .files_glob
                        .clone()
                        .or_else(|| existing.and_then(|e| e.files_glob.clone()));

                    // stream is explicit in rich config, no fallback to existing
                    let stream = rich.stream;

                    // cache config from rich format takes precedence
                    let cache = rich
                        .cache
                        .clone()
                        .or_else(|| existing.and_then(|e| e.cache.clone()));

                    // invalidates_cache from rich config (no fallback - explicit only)
                    let invalidates_cache = rich.invalidates_cache;

                    // working_dir preserved from existing (not configurable via aster.toml)
                    let working_dir = existing.and_then(|e| e.working_dir.clone());

                    targets.insert(
                        name.clone(),
                        Target {
                            command: rich.command.clone(),
                            depends_on,
                            capabilities,
                            files_glob,
                            stream,
                            cache,
                            invalidates_cache,
                            working_dir,
                        },
                    );
                }
                TargetConfig::Alias(_) => {
                    // Handled in second pass
                }
            }
        }

        // Second pass: resolve alias targets by cloning from already-resolved targets
        for (name, config) in custom_targets {
            if let TargetConfig::Alias(alias_config) = config {
                // Resolve the alias reference: strip //self: prefix if present
                let aliased_name = alias_config
                    .alias
                    .strip_prefix("//self:")
                    .unwrap_or(&alias_config.alias);

                if let Some(source_target) = targets.get(aliased_name).cloned() {
                    // Clone the source target and append extra depends_on
                    let mut depends_on = source_target.depends_on.clone();
                    if !alias_config.depends_on.is_empty() {
                        let extra_deps = Self::resolve_self_references(
                            &alias_config.depends_on,
                            project_address,
                        );
                        depends_on.extend(extra_deps);
                    }

                    targets.insert(
                        name.clone(),
                        Target {
                            command: source_target.command,
                            depends_on,
                            capabilities: source_target.capabilities,
                            files_glob: source_target.files_glob,
                            stream: source_target.stream,
                            cache: source_target.cache,
                            invalidates_cache: source_target.invalidates_cache,
                            working_dir: source_target.working_dir,
                        },
                    );
                } else {
                    eprintln!(
                        "Warning: alias target '{name}' references non-existent target '{aliased_name}', skipping"
                    );
                }
            }
        }

        targets
    }

    /// Resolve //self: references to the actual project address
    fn resolve_self_references(deps: &[String], project_address: &str) -> Vec<String> {
        deps.iter()
            .map(|dep| {
                if let Some(target_name) = dep.strip_prefix("//self:") {
                    format!("{project_address}:{target_name}")
                } else {
                    dep.clone()
                }
            })
            .collect()
    }

    /// Parse capability strings into TargetCapability enum values
    fn parse_capabilities(caps: &[String]) -> HashSet<TargetCapability> {
        caps.iter()
            .filter_map(|cap| match cap.as_str() {
                "files_list" => Some(TargetCapability::FilesList),
                _ => None, // Unknown capabilities are ignored
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RichTargetConfig;

    fn target(command: &str, depends_on: Vec<&str>) -> Target {
        Target {
            command: command.to_string(),
            depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
            capabilities: HashSet::new(),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
        }
    }

    fn simple(command: &str) -> TargetConfig {
        TargetConfig::Simple(command.to_string())
    }

    fn rich(
        command: &str,
        depends_on: Vec<&str>,
        capabilities: Vec<&str>,
        files_glob: Option<&str>,
    ) -> TargetConfig {
        TargetConfig::Rich(RichTargetConfig {
            command: command.to_string(),
            depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
            capabilities: capabilities.into_iter().map(|s| s.to_string()).collect(),
            files_glob: files_glob.map(|s| s.to_string()),
            stream: false,
            cache: None,
            invalidates_cache: false,
        })
    }

    #[test]
    fn test_detected_targets_only() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));
        detected.insert(
            "build".to_string(),
            target("npm run build", vec!["//self:deps"]),
        );

        let targets = TargetResolver::resolve(&detected, &HashMap::new(), "//apps/web");

        assert_eq!(
            targets.get(TARGET_TEST).map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            targets.get(TARGET_BUILD).map(|t| &t.command),
            Some(&"npm run build".to_string())
        );
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
    fn test_simple_override_preserves_depends_on() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));
        detected.insert(
            "build".to_string(),
            target("npm run build", vec!["//self:deps"]),
        );

        let mut custom = HashMap::new();
        custom.insert("test".to_string(), simple("npm run test:ci"));

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        // test command is overridden but depends_on is preserved
        let test_target = targets.get(TARGET_TEST).unwrap();
        assert_eq!(test_target.command, "npm run test:ci");
        assert_eq!(test_target.depends_on, vec!["//apps/web:deps".to_string()]);

        // build keeps detected values
        assert_eq!(
            targets.get(TARGET_BUILD).map(|t| &t.command),
            Some(&"npm run build".to_string())
        );
    }

    #[test]
    fn test_simple_addition() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec![]));

        let mut custom = HashMap::new();
        custom.insert("deploy".to_string(), simple("npm run deploy"));

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        // Detected target still there
        assert_eq!(
            targets.get(TARGET_TEST).map(|t| &t.command),
            Some(&"npm test".to_string())
        );
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
        custom.insert("test".to_string(), simple("make test"));

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
    fn test_simple_completely_overrides_detected() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("pytest", vec!["//self:deps"]));
        detected.insert(
            "lint".to_string(),
            target("ruff check .", vec!["//self:deps"]),
        );

        let mut custom = HashMap::new();
        custom.insert("test".to_string(), simple("python -m pytest --cov"));
        custom.insert("lint".to_string(), simple("mypy ."));
        custom.insert("typecheck".to_string(), simple("pyright ."));

        let targets = TargetResolver::resolve(&detected, &custom, "//libs/ml");

        // Commands are overridden
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"python -m pytest --cov".to_string())
        );
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"mypy .".to_string())
        );
        // But depends_on is preserved for existing targets
        assert_eq!(
            targets.get("test").unwrap().depends_on,
            vec!["//libs/ml:deps".to_string()]
        );
        assert_eq!(
            targets.get("lint").unwrap().depends_on,
            vec!["//libs/ml:deps".to_string()]
        );
        // New target has empty depends_on
        assert!(targets.get("typecheck").unwrap().depends_on.is_empty());
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn test_rich_target_with_all_fields() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));

        let mut custom = HashMap::new();
        custom.insert(
            "test".to_string(),
            rich(
                "pytest {files}",
                vec!["//self:deps", "//libs/shared:build"],
                vec!["files_list"],
                Some("*_test.py"),
            ),
        );

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/api");

        let test_target = targets.get("test").unwrap();
        assert_eq!(test_target.command, "pytest {files}");
        assert_eq!(
            test_target.depends_on,
            vec!["//apps/api:deps", "//libs/shared:build"]
        );
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));
        assert_eq!(test_target.files_glob, Some("*_test.py".to_string()));
    }

    #[test]
    fn test_rich_target_preserves_unspecified_fields() {
        let mut detected = HashMap::new();
        let mut detected_target = target("npm test", vec!["//self:deps"]);
        detected_target
            .capabilities
            .insert(TargetCapability::FilesList);
        detected_target.files_glob = Some("*.test.ts".to_string());
        detected.insert("test".to_string(), detected_target);

        let mut custom = HashMap::new();
        // Rich format with only command - should preserve depends_on, capabilities, files_glob
        custom.insert(
            "test".to_string(),
            rich("npm run test:ci", vec![], vec![], None),
        );

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        let test_target = targets.get("test").unwrap();
        assert_eq!(test_target.command, "npm run test:ci");
        // These should be preserved from detected
        assert_eq!(test_target.depends_on, vec!["//apps/web:deps".to_string()]);
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));
        assert_eq!(test_target.files_glob, Some("*.test.ts".to_string()));
    }

    #[test]
    fn test_rich_target_new_target() {
        let detected = HashMap::new();

        let mut custom = HashMap::new();
        custom.insert(
            "integration".to_string(),
            rich(
                "go test -tags=integration ./...",
                vec!["//self:build"],
                vec!["files_list"],
                Some("*_test.go"),
            ),
        );

        let targets = TargetResolver::resolve(&detected, &custom, "//services/api");

        let integration_target = targets.get("integration").unwrap();
        assert_eq!(
            integration_target.command,
            "go test -tags=integration ./..."
        );
        assert_eq!(
            integration_target.depends_on,
            vec!["//services/api:build".to_string()]
        );
        assert!(integration_target
            .capabilities
            .contains(&TargetCapability::FilesList));
        assert_eq!(integration_target.files_glob, Some("*_test.go".to_string()));
    }

    fn alias(alias_name: &str, depends_on: Vec<&str>) -> TargetConfig {
        TargetConfig::Alias(crate::config::AliasTargetConfig {
            alias: alias_name.to_string(),
            depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn test_alias_clones_target() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));

        let mut custom = HashMap::new();
        custom.insert("check".to_string(), alias("test", vec![]));

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        let check = targets.get("check").unwrap();
        assert_eq!(check.command, "npm test");
        assert_eq!(check.depends_on, vec!["//apps/web:deps".to_string()]);
    }

    #[test]
    fn test_alias_with_self_prefix() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("pytest", vec![]));

        let mut custom = HashMap::new();
        custom.insert("check".to_string(), alias("//self:test", vec![]));

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/api");

        let check = targets.get("check").unwrap();
        assert_eq!(check.command, "pytest");
    }

    #[test]
    fn test_alias_with_extra_deps() {
        let mut detected = HashMap::new();
        detected.insert("test".to_string(), target("npm test", vec!["//self:deps"]));

        let mut custom = HashMap::new();
        custom.insert(
            "ci".to_string(),
            alias("test", vec!["//self:lint", "//self:typecheck"]),
        );

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        let ci = targets.get("ci").unwrap();
        assert_eq!(ci.command, "npm test");
        assert_eq!(
            ci.depends_on,
            vec![
                "//apps/web:deps".to_string(),
                "//apps/web:lint".to_string(),
                "//apps/web:typecheck".to_string(),
            ]
        );
    }

    #[test]
    fn test_alias_of_nonexistent_target_skipped() {
        let detected = HashMap::new();

        let mut custom = HashMap::new();
        custom.insert("check".to_string(), alias("nonexistent", vec![]));

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/web");

        // Alias to non-existent target should be silently skipped
        assert!(targets.get("check").is_none());
    }

    #[test]
    fn test_alias_of_custom_rich_target() {
        let detected = HashMap::new();

        let mut custom = HashMap::new();
        custom.insert(
            "test".to_string(),
            rich(
                "pytest {files}",
                vec!["//self:deps"],
                vec!["files_list"],
                Some("*_test.py"),
            ),
        );
        custom.insert("check".to_string(), alias("test", vec![]));

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/api");

        let check = targets.get("check").unwrap();
        assert_eq!(check.command, "pytest {files}");
        assert_eq!(check.depends_on, vec!["//apps/api:deps".to_string()]);
        assert!(check.capabilities.contains(&TargetCapability::FilesList));
        assert_eq!(check.files_glob, Some("*_test.py".to_string()));
    }

    #[test]
    fn test_unknown_capability_ignored() {
        let detected = HashMap::new();

        let mut custom = HashMap::new();
        custom.insert(
            "test".to_string(),
            rich(
                "pytest",
                vec![],
                vec!["files_list", "unknown_cap", "another_unknown"],
                None,
            ),
        );

        let targets = TargetResolver::resolve(&detected, &custom, "//apps/api");

        let test_target = targets.get("test").unwrap();
        // Only files_list should be in capabilities
        assert_eq!(test_target.capabilities.len(), 1);
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));
    }
}
