use super::{
    ElixirPlugin, GoPlugin, GradlePlugin, LanguagePlugin, MavenPlugin, NodeJsPlugin, PythonPlugin,
    RubyPlugin, RustPlugin,
};

/// Registry of available language plugins
pub struct PluginRegistry {
    plugins: Vec<Box<dyn LanguagePlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Create a registry with all built-in language plugins pre-registered
    pub fn with_all_plugins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(NodeJsPlugin));
        registry.register(Box::new(ElixirPlugin));
        registry.register(Box::new(PythonPlugin));
        registry.register(Box::new(GoPlugin));
        registry.register(Box::new(RustPlugin));
        registry.register(Box::new(GradlePlugin));
        registry.register(Box::new(MavenPlugin));
        registry.register(Box::new(RubyPlugin));
        registry
    }

    /// Register a plugin
    pub fn register(&mut self, plugin: Box<dyn LanguagePlugin>) {
        self.plugins.push(plugin);
    }

    /// Get all registered plugins
    pub fn plugins(&self) -> &[Box<dyn LanguagePlugin>] {
        &self.plugins
    }

    /// Find plugin that handles a given marker file
    pub fn find_by_marker(&self, filename: &str) -> Option<&dyn LanguagePlugin> {
        self.plugins
            .iter()
            .find(|p| p.matches_marker(filename))
            .map(|p| p.as_ref())
    }

    /// Find plugin by name
    pub fn find_by_name(&self, name: &str) -> Option<&dyn LanguagePlugin> {
        self.plugins
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.as_ref())
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{LocalDependency, ProjectMetadata, Target, TargetContext};
    use anyhow::Result;
    use std::collections::HashMap;
    use std::path::Path;

    // Mock plugin for testing
    struct MockPlugin {
        name: &'static str,
        markers: Vec<&'static str>,
    }

    impl LanguagePlugin for MockPlugin {
        fn name(&self) -> &str {
            self.name
        }

        fn marker_files(&self) -> &[&str] {
            &self.markers
        }

        fn parse_project(&self, _root: &Path, _config_path: &Path) -> Result<ProjectMetadata> {
            Ok(ProjectMetadata {
                name: "mock-project".to_string(),
                version: Some("1.0.0".to_string()),
            })
        }

        fn parse_dependencies(&self, _config_path: &Path) -> Result<Vec<LocalDependency>> {
            Ok(vec![])
        }

        fn detect_targets(&self, _ctx: &TargetContext) -> Result<HashMap<String, Target>> {
            Ok(HashMap::new())
        }
    }

    #[test]
    fn test_register_and_find() {
        let mut registry = PluginRegistry::new();

        let plugin = MockPlugin {
            name: "nodejs",
            markers: vec!["package.json"],
        };
        registry.register(Box::new(plugin));

        let found = registry.find_by_marker("package.json");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name(), "nodejs");
    }

    #[test]
    fn test_find_unknown_marker() {
        let registry = PluginRegistry::new();
        let found = registry.find_by_marker("unknown.file");
        assert!(found.is_none());
    }

    #[test]
    fn test_all_plugins_register_and_find() {
        use crate::plugins::{ElixirPlugin, NodeJsPlugin, PythonPlugin, RustPlugin};

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(NodeJsPlugin));
        registry.register(Box::new(ElixirPlugin));
        registry.register(Box::new(PythonPlugin));
        registry.register(Box::new(RustPlugin));

        // Verify all four plugins are registered
        assert_eq!(registry.plugins().len(), 4);

        // Verify each can be found by its marker file
        let nodejs = registry.find_by_marker("package.json");
        assert!(nodejs.is_some());
        assert_eq!(nodejs.unwrap().name(), "nodejs");

        let elixir = registry.find_by_marker("mix.exs");
        assert!(elixir.is_some());
        assert_eq!(elixir.unwrap().name(), "elixir");

        let python = registry.find_by_marker("pyproject.toml");
        assert!(python.is_some());
        assert_eq!(python.unwrap().name(), "python");

        let rust = registry.find_by_marker("Cargo.toml");
        assert!(rust.is_some());
        assert_eq!(rust.unwrap().name(), "rust");

        // Unknown marker returns None
        assert!(registry.find_by_marker("unknown.toml").is_none());
    }

    #[test]
    fn test_find_by_name() {
        use crate::plugins::{ElixirPlugin, NodeJsPlugin, PythonPlugin, RustPlugin};

        let mut registry = PluginRegistry::new();
        registry.register(Box::new(NodeJsPlugin));
        registry.register(Box::new(ElixirPlugin));
        registry.register(Box::new(PythonPlugin));
        registry.register(Box::new(RustPlugin));

        // Find by name
        let nodejs = registry.find_by_name("nodejs");
        assert!(nodejs.is_some());
        assert_eq!(nodejs.unwrap().name(), "nodejs");

        let elixir = registry.find_by_name("elixir");
        assert!(elixir.is_some());
        assert_eq!(elixir.unwrap().name(), "elixir");

        let python = registry.find_by_name("python");
        assert!(python.is_some());
        assert_eq!(python.unwrap().name(), "python");

        let rust = registry.find_by_name("rust");
        assert!(rust.is_some());
        assert_eq!(rust.unwrap().name(), "rust");

        // Unknown name returns None
        assert!(registry.find_by_name("unknown").is_none());
        assert!(registry.find_by_name("").is_none());
    }

    #[test]
    fn all_builtin_plugins_are_registered() {
        let registry = PluginRegistry::with_all_plugins();
        assert_eq!(registry.plugins().len(), 8);
        assert_eq!(registry.find_by_marker("pom.xml").unwrap().name(), "maven");
        assert_eq!(
            registry.find_by_marker("build.gradle.kts").unwrap().name(),
            "gradle"
        );
        assert_eq!(registry.find_by_marker("Gemfile").unwrap().name(), "ruby");
        assert_eq!(
            registry.find_by_marker("sample.gemspec").unwrap().name(),
            "ruby"
        );
        assert_eq!(
            registry
                .find_by_marker("custom-project.gradle.kts")
                .unwrap()
                .name(),
            "gradle"
        );
    }
}
