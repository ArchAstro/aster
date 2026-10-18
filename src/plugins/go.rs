//! Go language plugin for go.mod parsing

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{
    LanguagePlugin, LocalDependency, NativeCache, ProjectMetadata, Target, TargetCapability,
    TargetContext,
};

/// Regex to extract module path from go.mod: `module github.com/user/project`
static MODULE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^module\s+(\S+)").expect("Invalid MODULE_REGEX"));

/// Regex to extract local replace directives: `replace old => ../path`
/// Captures: (1) module being replaced, (2) local path (starting with . or /)
static REPLACE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^replace\s+(\S+)(?:\s+\S+)?\s+=>\s+(\.\.?[^\s]+|/[^\s]+)")
        .expect("Invalid REPLACE_REGEX")
});

/// Go plugin for discovering and parsing Go modules
pub struct GoPlugin;

impl LanguagePlugin for GoPlugin {
    fn name(&self) -> &str {
        "go"
    }

    fn marker_files(&self) -> &[&str] {
        &["go.mod"]
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let name = MODULE_REGEX
            .captures(&content)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| {
                anyhow!(
                    "Could not find module declaration in {}",
                    config_path.display()
                )
            })?;

        // Extract just the last component of the module path as the project name
        // e.g., "github.com/user/myproject" -> "myproject"
        let short_name = name.rsplit('/').next().unwrap_or(&name).to_string();

        Ok(ProjectMetadata {
            name: short_name,
            version: None,
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let mut deps = Vec::new();

        // Extract local replace directives
        for caps in REPLACE_REGEX.captures_iter(&content) {
            let module_name = caps.get(1).unwrap().as_str();
            let path_str = caps.get(2).unwrap().as_str();

            deps.push(LocalDependency {
                name: module_name.to_string(),
                path: PathBuf::from(path_str),
            });
        }

        Ok(deps)
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let mut targets = HashMap::new();

        // deps target: download Go dependencies
        targets.insert(
            "deps".to_string(),
            Target {
                command: "go mod download".to_string(),
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
        // - //self:deps (install our own dependencies first)
        // - :build for each project dependency (they must be built first)
        let mut base_deps = vec!["//self:deps".to_string()];
        for dep_addr in &dependency_addresses {
            base_deps.push(format!("{dep_addr}:build"));
        }

        // build target: compile all packages
        targets.insert(
            "build".to_string(),
            Target {
                command: "go build ./...".to_string(),
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

        // test target: run all tests
        let mut test_caps = HashSet::new();
        test_caps.insert(TargetCapability::FilesList);
        targets.insert(
            "test".to_string(),
            Target {
                command: "go test ./...".to_string(),
                depends_on: base_deps.clone(),
                capabilities: test_caps,
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,
                working_dir: None,
                exclusive_resources: vec![],
                retry_setup: None,
            },
        );

        // format target: always available (built-in)
        targets.insert(
            "format".to_string(),
            Target {
                command: "go fmt ./...".to_string(),
                depends_on: vec!["//self:deps".to_string(), "//self:build".to_string()],
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

        // lint target: only if golangci-lint config exists
        let lint_configs = [
            ".golangci.yml",
            ".golangci.yaml",
            ".golangci.toml",
            ".golangci.json",
        ];
        let has_lint_config = lint_configs
            .iter()
            .any(|config| ctx.project_dir.join(config).exists());

        if has_lint_config {
            targets.insert(
                "lint".to_string(),
                Target {
                    command: "golangci-lint run".to_string(),
                    depends_on: base_deps,
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
        }

        // Add clean target
        if let Some(clean) = self.clean_target(ctx) {
            targets.insert("clean".to_string(), clean);
        }

        Ok(targets)
    }

    fn related_tests(
        &self,
        _project_dir: &Path,
        command: &str,
        tests: &[super::RelatedTest],
    ) -> Option<super::FilesListPlan> {
        super::related_tests::go(command, tests)
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

        // Filter to Go test files only
        let test_files: Vec<&PathBuf> = files
            .iter()
            .filter(|f| {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                name.ends_with("_test.go")
            })
            .collect();

        if test_files.is_empty() {
            // No test files in the change set - run full test suite
            return None;
        }

        // Extract unique directories containing test files
        // Go test runs on packages (directories), not individual files
        let mut test_dirs: Vec<String> = test_files
            .iter()
            .filter_map(|f| {
                f.parent()
                    .map(|p| crate::executor::quote_command_argument(&format!("./{}", p.display())))
            })
            .collect();
        test_dirs.sort();
        test_dirs.dedup();

        if test_dirs.is_empty() {
            return None;
        }

        // Replace ./... with specific directories
        let base_command = command.trim_end_matches("./...");
        Some(format!("{}{}", base_command, test_dirs.join(" ")))
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        // deps target only cares about dependency specification files,
        // not source code — source changes shouldn't re-download modules
        if target_name == "deps" {
            return super::CacheInputs {
                source_globs: vec![],
                config_files: vec!["go.mod".to_string(), "go.sum".to_string()],
                env_vars: vec!["GOOS".to_string(), "GOARCH".to_string()],
                runtime_commands: vec!["go version".to_string()],
                ..Default::default()
            };
        }

        // Go test files (*_test.go) are already included by **/*.go
        super::CacheInputs {
            source_globs: vec!["**/*.go".to_string()],
            config_files: vec!["go.mod".to_string(), "go.sum".to_string()],
            env_vars: vec![
                "GOOS".to_string(),
                "GOARCH".to_string(),
                "CGO_ENABLED".to_string(),
            ],
            runtime_commands: vec!["go version".to_string()],
            ..Default::default()
        }
    }

    fn native_caches(&self, ctx: &TargetContext) -> Vec<NativeCache> {
        // Go modules share one download cache. The build cache path is
        // whatever `go env GOCACHE` prints; cache-plan runs that command.
        // The plugin does not, so the path stays a command substitution.
        let sum = join_workspace_relative(
            &workspace_relative(ctx.project_dir, ctx.workspace_root),
            "go.sum",
        );
        vec![
            NativeCache {
                id: "go-mod".to_string(),
                paths: vec!["~/go/pkg/mod".to_string()],
                key_files: vec![sum],
                key_runtime: None,
                shared: true,
            },
            NativeCache {
                id: "go-build".to_string(),
                paths: vec!["$(go env GOCACHE)".to_string()],
                key_files: vec![],
                key_runtime: Some("go version".to_string()),
                shared: true,
            },
        ]
    }

    fn default_outputs(
        &self,
        target_name: &str,
        command: &str,
        _ctx: &TargetContext,
    ) -> Vec<String> {
        if target_name != "build" {
            return Vec::new();
        }
        // `go build ./...` writes no binary. A command that contains
        // `-o <path>` names the binary the cache should store.
        go_output_flag(command)
            .map(|path| vec![path])
            .unwrap_or_default()
    }

    fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
        Some(Target {
            command: "go clean".to_string(),
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
}

/// Path after a Go `-o` flag, when the command writes one binary.
///
/// Accepts `-o path` and `-o./path`. The path must be project-relative: an
/// absolute path, a `..` segment, or a flag-looking value stores nothing.
fn go_output_flag(command: &str) -> Option<String> {
    let tokens = split_command_tokens(command);
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        let path = if token == "-o" {
            index += 1;
            tokens.get(index).map(String::as_str)
        } else if is_inline_output_flag(token) {
            Some(&token[2..])
        } else {
            None
        };
        if let Some(path) = path {
            return project_relative_output(path);
        }
        index += 1;
    }
    None
}

/// `-o./path` is Go's inline `-o` flag. `-overlay` is a different flag.
fn is_inline_output_flag(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() > 2 && bytes[1] == b'o' && !bytes[2].is_ascii_alphabetic()
}

fn split_command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match quote {
            Some(mark) if ch == mark => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn project_relative_output(path: &str) -> Option<String> {
    if path.is_empty() || path.starts_with('-') || path.starts_with('/') {
        return None;
    }
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return None;
    }
    Some(path.replace('\\', "/"))
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

/// Path relative to the Aster workspace root, with `/` separators.
fn workspace_relative(path: &Path, workspace_root: &Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn join_workspace_relative(root: &str, child: &str) -> String {
    if root.is_empty() {
        child.to_string()
    } else {
        format!("{root}/{child}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn default_outputs_use_the_go_output_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(&go_mod, "module example.com/app\n").unwrap();
        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, tmp.path(), &[]);

        assert!(plugin
            .default_outputs("build", "go build ./...", &ctx)
            .is_empty());
        assert_eq!(
            plugin.default_outputs("build", "go build -o bin/app ./cmd/app", &ctx),
            vec!["bin/app"]
        );
        assert_eq!(
            plugin.default_outputs("build", "go build -o./bin/app", &ctx),
            vec!["./bin/app"]
        );
        assert!(plugin
            .default_outputs("build", "go build -o /tmp/app", &ctx)
            .is_empty());
        assert!(plugin
            .default_outputs("build", "go build -o ../app", &ctx)
            .is_empty());
        assert!(plugin
            .default_outputs("build", "go build -overlay overlay.json", &ctx)
            .is_empty());
        for name in ["test", "lint", "format", "check"] {
            assert!(
                plugin
                    .default_outputs(name, "go test -o bin/app", &ctx)
                    .is_empty(),
                "{name}"
            );
        }
    }

    #[test]
    fn test_parse_simple_go_mod() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module github.com/myorg/myproject

go 1.21
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let metadata = plugin.parse_project(tmp.path(), &go_mod).unwrap();

        assert_eq!(metadata.name, "myproject");
        assert_eq!(metadata.version, None);
    }

    #[test]
    fn test_parse_simple_module_name() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let metadata = plugin.parse_project(tmp.path(), &go_mod).unwrap();

        assert_eq!(metadata.name, "myapp");
    }

    #[test]
    fn test_parse_replace_directive() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21

require (
    github.com/myorg/shared v1.0.0
)

replace github.com/myorg/shared => ../shared
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let deps = plugin.parse_dependencies(&go_mod).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "github.com/myorg/shared");
        assert_eq!(deps[0].path, PathBuf::from("../shared"));
    }

    #[test]
    fn test_parse_replace_with_version() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21

replace github.com/myorg/shared v1.0.0 => ../shared
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let deps = plugin.parse_dependencies(&go_mod).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "github.com/myorg/shared");
        assert_eq!(deps[0].path, PathBuf::from("../shared"));
    }

    #[test]
    fn test_parse_multiple_replace_directives() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21

replace github.com/myorg/shared => ../shared
replace github.com/myorg/utils => ./internal/utils
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let deps = plugin.parse_dependencies(&go_mod).unwrap();

        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"github.com/myorg/shared"));
        assert!(names.contains(&"github.com/myorg/utils"));
    }

    #[test]
    fn test_parse_non_local_replace_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21

replace github.com/old/pkg => github.com/new/pkg v1.2.3
replace github.com/myorg/local => ../local
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let deps = plugin.parse_dependencies(&go_mod).unwrap();

        // Only local replace should be captured
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "github.com/myorg/local");
    }

    #[test]
    fn test_missing_module() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(&go_mod, "go 1.21\n").unwrap();

        let plugin = GoPlugin;
        let result = plugin.parse_project(tmp.path(), &go_mod);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Could not find module declaration"));
    }

    #[test]
    fn test_detect_targets_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"go mod download".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"go build ./...".to_string())
        );
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"go test ./...".to_string())
        );
        assert!(!targets.contains_key("lint")); // No lint config

        // Check dependencies
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
        assert_eq!(
            targets.get("build").unwrap().depends_on,
            vec!["//self:deps"]
        );
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//self:deps"]);
        assert_eq!(
            plugin.cache_inputs("build").runtime_commands,
            vec!["go version".to_string()]
        );
    }

    #[test]
    fn test_detect_targets_has_format() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format is always available for Go projects
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"go fmt ./...".to_string())
        );

        // Check dependencies - only self:deps and self:build
        let format_deps = &targets.get("format").unwrap().depends_on;
        assert!(format_deps.contains(&"//self:deps".to_string()));
        assert!(format_deps.contains(&"//self:build".to_string()));
        assert_eq!(format_deps.len(), 2);

        // No FilesList capability
        assert!(!targets
            .get("format")
            .unwrap()
            .capabilities
            .contains(&TargetCapability::FilesList));
    }

    #[test]
    fn test_detect_targets_with_lint_config() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        // Create golangci-lint config
        std::fs::write(
            tmp.path().join(".golangci.yml"),
            "linters:\n  enable:\n    - gofmt\n",
        )
        .unwrap();

        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"golangci-lint run".to_string())
        );
    }

    #[test]
    fn test_detect_targets_with_project_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize workspace root to handle macOS /var -> /private/var symlink
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create workspace structure
        let project_dir = workspace_root.join("cmd/api");
        std::fs::create_dir_all(&project_dir).unwrap();
        let go_mod = project_dir.join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        // Create dependency directories (needed for canonicalize)
        let shared_dir = workspace_root.join("pkg/shared");
        std::fs::create_dir_all(&shared_dir).unwrap();

        // Dependencies with relative paths
        let dependencies = vec![LocalDependency {
            name: "github.com/myorg/shared".to_string(),
            path: "../../pkg/shared".into(),
        }];

        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, &workspace_root, &dependencies);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // build should depend on deps and build of each dependency
        let build_deps = &targets.get("build").unwrap().depends_on;
        assert!(build_deps.contains(&"//self:deps".to_string()));
        assert!(build_deps.contains(&"//pkg/shared:build".to_string()));
    }

    #[test]
    fn test_with_files_list_filters_test_files() {
        let plugin = GoPlugin;
        let files = vec![
            PathBuf::from("main.go"),
            PathBuf::from("handler.go"),
            PathBuf::from("handler_test.go"),
            PathBuf::from("pkg/utils/utils.go"),
            PathBuf::from("pkg/utils/utils_test.go"),
            PathBuf::from("go.mod"),
        ];

        let result = plugin.with_files_list("test", "go test ./...", &files);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.starts_with("go test "));
        // Root dir files produce "./" and subdirs produce "./pkg/utils"
        // Check that we have both patterns
        assert!(cmd.contains("./") && !cmd.contains("./..."));
        assert!(cmd.contains("./pkg/utils"));
    }

    #[test]
    fn test_with_files_list_returns_none_for_non_test_target() {
        let plugin = GoPlugin;
        let files = vec![PathBuf::from("handler_test.go")];

        let result = plugin.with_files_list("build", "go build ./...", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_returns_none_when_no_test_files() {
        let plugin = GoPlugin;
        let files = vec![
            PathBuf::from("main.go"),
            PathBuf::from("handler.go"),
            PathBuf::from("go.mod"),
        ];

        let result = plugin.with_files_list("test", "go test ./...", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_test_target_has_files_list_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));

        let build_target = targets.get("build").unwrap();
        assert!(!build_target
            .capabilities
            .contains(&TargetCapability::FilesList));
    }

    #[test]
    fn test_detect_targets_has_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let go_mod = tmp.path().join("go.mod");
        std::fs::write(
            &go_mod,
            r#"module myapp

go 1.21
"#,
        )
        .unwrap();

        let plugin = GoPlugin;
        let ctx = make_context(&go_mod, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert_eq!(clean.command, "go clean");
        assert!(clean.invalidates_cache);
    }

    #[test]
    fn test_native_caches_module() {
        let tmp = tempfile::tempdir().unwrap();
        let module = tmp.path().join("services/api");
        std::fs::create_dir_all(&module).unwrap();
        let go_mod = module.join("go.mod");
        std::fs::write(&go_mod, "module example.com/api\n\ngo 1.22\n").unwrap();
        std::fs::write(module.join("go.sum"), "").unwrap();
        let ctx = make_context(&go_mod, tmp.path(), &[]);

        let caches = GoPlugin.native_caches(&ctx);

        assert_eq!(caches.len(), 2);
        assert_eq!(caches[0].id, "go-mod");
        assert_eq!(caches[0].paths, vec!["~/go/pkg/mod".to_string()]);
        assert_eq!(caches[0].key_files, vec!["services/api/go.sum".to_string()]);
        assert_eq!(caches[0].key_runtime, None);
        assert!(caches[0].shared);

        assert_eq!(caches[1].id, "go-build");
        assert_eq!(caches[1].paths, vec!["$(go env GOCACHE)".to_string()]);
        assert!(caches[1].key_files.is_empty());
        assert_eq!(caches[1].key_runtime.as_deref(), Some("go version"));
        assert!(caches[1].shared);
    }
}
