//! Go language plugin for go.mod parsing

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::related_tests::GoTest;
use super::{
    FilesListPlan, LanguagePlugin, LocalDependency, NativeCache, ProjectMetadata, RelatedTest,
    Target, TargetCapability, TargetContext,
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
                selected_by: vec![],
                retry_setup: None,
                report: None,
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
                selected_by: vec![],
                retry_setup: None,
                report: None,
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
                selected_by: vec![],
                retry_setup: None,
                report: None,
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
                selected_by: vec![],
                retry_setup: None,
                report: None,
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
                    selected_by: vec![],
                    retry_setup: None,
                    report: None,
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
        project_dir: &Path,
        command: &str,
        tests: &[RelatedTest],
    ) -> Option<FilesListPlan> {
        deferred_go_tests(project_dir, command, tests)
            .or_else(|| related_go_tests(project_dir, command, tests, true))
    }

    fn related_tests_in_hidden_environment(
        &self,
        project_dir: &Path,
        command: &str,
        tests: &[RelatedTest],
    ) -> Option<FilesListPlan> {
        deferred_go_tests(project_dir, command, tests)
            .or_else(|| related_go_tests(project_dir, command, tests, false))
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
            selected_by: vec![],
            retry_setup: None,
            report: None,
        })
    }
}

/// An explicit external execution boundary, not recognition of arbitrary setup
/// syntax. The payload records absolute files so a sourced setup may change cwd.
fn deferred_go_tests(
    project_dir: &Path,
    command: &str,
    tests: &[RelatedTest],
) -> Option<FilesListPlan> {
    // This tail may be reinserted into a delegated shell script. Do not turn
    // expansions or operators into literal arguments. Direct execution does
    // not expand them either; declining narrowing keeps its argv unchanged.
    let parsed = super::related_tests::Argv::parse(command)?;
    if command.contains([
        '$', '`', '*', '?', '[', ']', '{', '}', '~', '|', '&', '<', '>', ';', '\n',
    ]) {
        return None;
    }
    let argv = parsed.parts;
    if argv.first()?.as_str() != "aster"
        || argv.get(1)?.as_str() != "exec-go-test"
        || argv.get(2)?.as_str() != "--"
    {
        return None;
    }
    let selection: Vec<_> = tests
        .iter()
        .map(|test| (project_dir.join(&test.file), &test.names))
        .collect();
    let payload = serde_json::to_string(&selection).ok()?;
    // A concurrent rebuild can unlink the running binary. On Linux,
    // current_exe can return a nonexistent path ending in " (deleted)".
    // Resolve the actual file or leave the original command untouched.
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    if !exe.is_file() {
        return None;
    }
    let quote = crate::executor::quote_command_argument;
    Some(FilesListPlan::Commands(vec![format!(
        "{} exec-go-test --selection {} -- {}",
        quote(&exe.to_string_lossy()),
        quote(&payload),
        argv[3..]
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    )]))
}

/// Execute external Go in the setup shell's exported environment and cwd.
/// No command string is evaluated by a shell. The parent shell stays alive,
/// including its EXIT trap, until this process returns.
pub fn exec_go_test(selection: Option<&str>, args: &[String]) -> Result<i32> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let quote = crate::executor::quote_command_argument;
    let original = format!(
        "go test {}",
        args.iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let plan = if let Some(selection) = selection {
        let selected: Vec<(PathBuf, Vec<String>)> =
            serde_json::from_str(selection).context("invalid deferred Go selection")?;
        let tests: Option<Vec<_>> = selected
            .into_iter()
            .map(|(file, names)| {
                Some(RelatedTest {
                    file: file
                        .canonicalize()
                        .ok()?
                        .strip_prefix(&cwd)
                        .ok()?
                        .to_path_buf(),
                    names,
                })
            })
            .collect();
        tests
            .and_then(|tests| related_go_tests(&cwd, &original, &tests, true))
            .unwrap_or(FilesListPlan::Full)
    } else {
        FilesListPlan::Full
    };
    let commands = match plan {
        FilesListPlan::Nothing => return Ok(0),
        FilesListPlan::Commands(commands) => commands,
        _ => vec![original],
    };
    for command in commands {
        let argv = shell_words::split(&command)?;
        eprintln!("[aster] external Go after setup: {command}");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .context("failed to run external Go tests")?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }
    }
    Ok(0)
}

/// Narrow `go test` to the selected tests that build under its build
/// context. When the environment is not fully known (`environment_known`
/// false), `go list` may see either more or fewer buildable files than the
/// command will. Preserve the original command in either direction.
fn related_go_tests(
    project_dir: &Path,
    command: &str,
    tests: &[RelatedTest],
    environment_known: bool,
) -> Option<FilesListPlan> {
    let go_test = GoTest::parse(command)?;
    if !environment_known || !go_test.context_is_static() {
        return Some(FilesListPlan::Full);
    }
    let selected: Vec<RelatedTest> = tests
        .iter()
        .filter(|test| go_test.covers(&GoTest::package_dir(test)))
        .cloned()
        .collect();
    if selected.is_empty() {
        return go_test.plan(&selected);
    }
    match buildable_tests(project_dir, &go_test, &selected) {
        Ok(Some(kept)) => go_test.plan(&kept),
        // Without a Go toolchain, buildability is unknown. Preserve the
        // original command rather than naming potentially excluded packages.
        Ok(None) => Some(FilesListPlan::Full),
        // A narrowed command that may name a package that does not build
        // would fail where the full command passes.
        Err(_) => Some(FilesListPlan::Full),
    }
}

/// One package as `go list -e -json` prints it: the fields that say which
/// test files build.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedPackage {
    dir: Option<PathBuf>,
    #[serde(default)]
    test_go_files: Vec<String>,
    #[serde(default)]
    x_test_go_files: Vec<String>,
}

/// The `tests` whose files build under the command's build context: its
/// build tags and other build flags, its environment assignments, and the
/// environment Aster runs it in. `go test ./...` only runs these, and a
/// package with none of its files in the build fails the run when named
/// ("build constraints exclude all Go files").
///
/// `go list` decides, so build constraints, file name suffixes such as
/// `_windows.go`, cgo and `-race` count as they do for `go test`. `Ok(None)`
/// means the Go program could not be started; `Err` means `go list` did not
/// describe the packages.
fn buildable_tests(
    project_dir: &Path,
    go_test: &GoTest,
    tests: &[RelatedTest],
) -> Result<Option<Vec<RelatedTest>>> {
    let dirs: std::collections::BTreeSet<String> = tests.iter().map(GoTest::package_dir).collect();
    let mut command = std::process::Command::new(go_test.program());
    command
        .arg("list")
        .args(go_test.build_context_flags())
        // `-find` reads each package's files without resolving its imports;
        // `-e` describes a package that does not build instead of failing.
        .args(["-find", "-e", "-json"])
        .args(dirs.iter().map(|dir| {
            if dir == "." {
                ".".to_string()
            } else {
                format!("./{dir}")
            }
        }))
        .envs(go_test.environment())
        // Which files build never depends on downloads, and planning must
        // not fetch modules (`-find` still does when go.sum lists them).
        .env("GOPROXY", "off")
        .current_dir(project_dir)
        .stdin(std::process::Stdio::null());
    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to run go list"),
    };
    if !output.status.success() {
        return Err(anyhow!(
            "go list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let root = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let mut building: HashMap<String, HashSet<String>> = HashMap::new();
    for package in serde_json::Deserializer::from_slice(&output.stdout).into_iter() {
        let package: ListedPackage = package.context("go list printed invalid JSON")?;
        let Some(dir) = package.dir else { continue };
        let dir = dir.canonicalize().unwrap_or(dir);
        let Ok(relative) = dir.strip_prefix(&root) else {
            continue;
        };
        let relative = workspace_relative(relative, Path::new(""));
        let relative = if relative.is_empty() {
            ".".to_string()
        } else {
            relative
        };
        building.entry(relative).or_default().extend(
            package
                .test_go_files
                .into_iter()
                .chain(package.x_test_go_files),
        );
    }
    // Every package asked about is described, even one that does not build.
    if let Some(missing) = dirs.iter().find(|dir| !building.contains_key(*dir)) {
        return Err(anyhow!("go list did not describe ./{missing}"));
    }
    Ok(Some(
        tests
            .iter()
            .filter(|test| {
                let name = test.file.file_name().map(|n| n.to_string_lossy());
                name.is_some_and(|name| building[&GoTest::package_dir(test)].contains(&*name))
            })
            .cloned()
            .collect(),
    ))
}

/// Path after the `-o` flag of a `go build` invocation in `command`.
///
/// The command is split with shell quoting rules. Only flags after a
/// `go build` word and before the next shell operator count, in Go's flag
/// forms `-o path`, `--o path`, `-o=path`, and `--o=path`. Unparseable
/// quoting, an absolute path, a `..` segment, or a flag-looking value stores
/// nothing, so a misread command never declares an output.
fn go_output_flag(command: &str) -> Option<String> {
    let tokens = shell_words::split(command).ok()?;
    let start = tokens.windows(2).position(|pair| {
        Path::new(&pair[0])
            .file_name()
            .is_some_and(|name| name == "go")
            && pair[1] == "build"
    })?;
    let mut rest = tokens[start + 2..]
        .iter()
        .take_while(|token| !matches!(token.as_str(), "&&" | "||" | ";" | "|" | "&"));
    while let Some(token) = rest.next() {
        let flag = token.strip_prefix("--").or_else(|| token.strip_prefix('-'));
        let path = match flag {
            Some("o") => rest.next().map(String::as_str),
            Some(flag) => flag.strip_prefix("o="),
            None => None,
        };
        if let Some(path) = path {
            return project_relative_output(path);
        }
    }
    None
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
        for command in [
            "go build -o=bin/app ./cmd/app",
            "go build --o bin/app",
            "go build --o=bin/app",
            "CGO_ENABLED=0 go build -trimpath -o 'bin/app' ./cmd/app && strip bin/app",
            "/usr/local/go/bin/go build -o bin/app",
        ] {
            assert_eq!(
                plugin.default_outputs("build", command, &ctx),
                vec!["bin/app"],
                "{command}"
            );
        }
        for command in [
            "go build -obin/app",
            "go build ./... && gcc -o bin/app main.c",
            "go vet -o bin/app",
            "go build -o 'bin/app",
            "go build -o",
        ] {
            assert!(
                plugin.default_outputs("build", command, &ctx).is_empty(),
                "{command} must not declare an output"
            );
        }
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
    fn deferred_tests_decline_shell_active_arguments() {
        for tail in [
            "-tags=$DEV_TAGS ./...",
            "./c*",
            "./c?rt",
            "./[c]art",
            "./... | cat",
            "./... >out",
            "$(echo ./...)",
            "{files}",
        ] {
            assert!(
                deferred_go_tests(
                    Path::new("."),
                    &format!("aster exec-go-test -- {tail}"),
                    &[]
                )
                .is_none(),
                "{tail}"
            );
        }
        assert!(deferred_go_tests(
            Path::new("."),
            "aster exec-go-test -- -tags=devtool ./...",
            &[]
        )
        .is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deferred_tests_decline_unlinked_current_executable() {
        const CHILD: &str = "ASTER_TEST_UNLINK_CURRENT_EXE";
        if std::env::var_os(CHILD).is_some() {
            std::fs::remove_file(std::env::current_exe().unwrap()).unwrap();
            assert!(
                deferred_go_tests(Path::new("."), "aster exec-go-test -- ./...", &[]).is_none()
            );
            return;
        }
        // Unlink only a disposable hard link, never Cargo's binary or another
        // run. A copy would be written through a file descriptor that a
        // concurrent test's fork can inherit, and exec of a file open for
        // writing fails with ETXTBSY; a hard link is never opened for writing.
        let current = std::env::current_exe().unwrap();
        let tmp = tempfile::tempdir_in(current.parent().unwrap()).unwrap();
        let exe = tmp.path().join("test-link");
        std::fs::hard_link(&current, &exe).unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "plugins::go::tests::deferred_tests_decline_unlinked_current_executable",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{output:?}"
        );
    }

    #[test]
    fn related_tests_without_go_preserve_the_original_command() {
        let tmp = tempfile::tempdir().unwrap();
        let command = format!("{} test ./...", tmp.path().join("go").display());
        let tests = [RelatedTest {
            file: PathBuf::from("devonly/main_test.go"),
            names: vec!["TestDev".to_string()],
        }];
        // No toolchain is needed to prove that an unavailable executable
        // leaves buildability unknown, for both direct and shell runners.
        for environment_known in [true, false] {
            assert_eq!(
                related_go_tests(tmp.path(), &command, &tests, environment_known),
                Some(FilesListPlan::Full)
            );
        }
    }

    /// Every package case of a module, each with one test, and how a
    /// related-tests run narrows `go test` over them. `go list` decides
    /// which packages build, so each row runs the real Go toolchain.
    #[test]
    fn related_tests_keep_only_the_test_files_the_build_context_builds() {
        if std::process::Command::new("go")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: go is not installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let test = |package: &str, name: &str| {
            format!("package {package}\n\nimport \"testing\"\n\nfunc {name}(t *testing.T) {{}}\n")
        };
        let tagged = |tag: &str, body: String| format!("//go:build {tag}\n\n{body}");
        let files = [
            ("go.mod", "module example.com/m\n\ngo 1.21\n".to_string()),
            // A test file in the module root package.
            ("root_test.go", test("m", "TestRoot")),
            ("plain/plain.go", "package plain\n".to_string()),
            ("plain/plain_test.go", test("plain", "TestPlain")),
            // Every file needs a build tag.
            (
                "devonly/main.go",
                tagged("devtag", "package main\n\nfunc main() {}\n".into()),
            ),
            (
                "devonly/main_test.go",
                tagged("devtag", test("main", "TestDev")),
            ),
            // Only test files: an in-package one and an external one.
            ("testonly/only_test.go", test("testonly", "TestOnly")),
            ("xtestonly/x_test.go", test("xtestonly_test", "TestX")),
            // Built only for another OS, by file name.
            ("otheros/os_plan9.go", "package otheros\n".to_string()),
            ("otheros/os_plan9_test.go", test("otheros", "TestPlan9")),
            // A package that builds, with one test file behind a tag.
            ("mixed/mixed.go", "package mixed\n".to_string()),
            ("mixed/a_test.go", test("mixed", "TestA")),
            ("mixed/b_test.go", tagged("devtag", test("mixed", "TestB"))),
            // Built only with the race detector, which sets the `race` tag.
            ("racey/racey.go", "package racey\n".to_string()),
            (
                "racey/racey_test.go",
                tagged("race", test("racey", "TestRace")),
            ),
            // Directories `./...` does not match.
            ("testdata/fx/fx_test.go", test("fx", "TestFixture")),
            ("_old/old_test.go", test("old", "TestOld")),
            (".hidden/h_test.go", test("h", "TestHidden")),
        ];
        for (path, content) in &files {
            let path = tmp.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let named = |file: &str, name: &str| RelatedTest {
            file: PathBuf::from(file),
            names: vec![name.to_string()],
        };
        let every: Vec<RelatedTest> = files
            .iter()
            .filter(|(path, _)| path.ends_with("_test.go"))
            .map(|(path, content)| {
                let name = content
                    .split("func ")
                    .nth(1)
                    .unwrap()
                    .split('(')
                    .next()
                    .unwrap();
                named(path, name)
            })
            .collect();
        let commands = |list: &[&str]| {
            Some(FilesListPlan::Commands(
                list.iter().map(|c| c.to_string()).collect(),
            ))
        };

        let rows: Vec<(&str, &str, Vec<RelatedTest>, Option<FilesListPlan>)> = vec![
            (
                "no tags: tag-gated, other-OS and race-only files and testdata, _ and . directories drop out",
                "go test ./...",
                every.clone(),
                commands(&["go test -run '^(TestA|TestOnly|TestPlain|TestRoot|TestX)$' . ./mixed ./plain ./testonly ./xtestonly"]),
            ),
            (
                "the command's -tags builds the tagged files",
                "go test -tags devtag ./...",
                every.clone(),
                commands(&["go test -tags devtag -run '^(TestA|TestB|TestDev|TestOnly|TestPlain|TestRoot|TestX)$' . ./devonly ./mixed ./plain ./testonly ./xtestonly"]),
            ),
            (
                "-tags=value and --tags forms",
                "go test --tags=devtag ./devonly",
                vec![named("devonly/main_test.go", "TestDev")],
                commands(&["go test --tags=devtag -run '^(TestDev)$' ./devonly"]),
            ),
            (
                "tags set through GOFLAGS on the command",
                "GOFLAGS=-tags=devtag go test ./devonly",
                vec![named("devonly/main_test.go", "TestDev")],
                commands(&["GOFLAGS=-tags=devtag go test -run '^(TestDev)$' ./devonly"]),
            ),
            (
                "a package whose only files are tag-gated runs nothing",
                "go test ./devonly",
                vec![named("devonly/main_test.go", "TestDev")],
                Some(FilesListPlan::Nothing),
            ),
            (
                "a whole-file selection in a tag-gated package drops out",
                "go test ./...",
                vec![
                    RelatedTest { file: PathBuf::from("devonly/main_test.go"), names: vec![] },
                    RelatedTest { file: PathBuf::from("plain/plain_test.go"), names: vec![] },
                ],
                commands(&["go test ./plain"]),
            ),
            (
                "a tag-gated test file in a package that builds drops out",
                "go test ./mixed",
                vec![named("mixed/b_test.go", "TestB")],
                Some(FilesListPlan::Nothing),
            ),
            (
                "a file for another OS drops out",
                "go test ./otheros",
                vec![named("otheros/os_plan9_test.go", "TestPlan9")],
                Some(FilesListPlan::Nothing),
            ),
            (
                // Plan 9 has no arm64 port: fix both axes so this probe
                // also succeeds on Apple Silicon CI runners.
                "GOOS and GOARCH on the command build that platform's files",
                "GOOS=plan9 GOARCH=amd64 go test ./otheros",
                vec![named("otheros/os_plan9_test.go", "TestPlan9")],
                commands(&["GOOS=plan9 GOARCH=amd64 go test -run '^(TestPlan9)$' ./otheros"]),
            ),
            (
                "an unsupported GOOS/GOARCH pair preserves the original command",
                "GOOS=plan9 GOARCH=arm64 go test ./otheros",
                vec![named("otheros/os_plan9_test.go", "TestPlan9")],
                Some(FilesListPlan::Full),
            ),
            (
                "-race sets the race build tag",
                "CGO_ENABLED=1 go test -race -count=1 ./racey",
                vec![named("racey/racey_test.go", "TestRace")],
                commands(&["CGO_ENABLED=1 go test -race -count=1 -run '^(TestRace)$' ./racey"]),
            ),
            (
                "a race-only file drops out without -race",
                "go test -count=1 ./racey",
                vec![named("racey/racey_test.go", "TestRace")],
                Some(FilesListPlan::Nothing),
            ),
            (
                "go list failing runs the command as written",
                "go test -mod=nonsense ./...",
                vec![named("plain/plain_test.go", "TestPlain")],
                Some(FilesListPlan::Full),
            ),
            (
                "without a Go program the command runs as written",
                "/nonexistent/go test ./...",
                vec![named("devonly/main_test.go", "TestDev")],
                Some(FilesListPlan::Full),
            ),
            (
                "tests outside the command's packages are not listed",
                "go test ./plain",
                vec![named("plain/plain_test.go", "TestPlain"), named("devonly/main_test.go", "TestDev")],
                commands(&["go test -run '^(TestPlain)$' ./plain"]),
            ),
            (
                "a testdata directory named on the command is listed",
                "go test ./testdata/fx",
                vec![named("testdata/fx/fx_test.go", "TestFixture")],
                commands(&["go test -run '^(TestFixture)$' ./testdata/fx"]),
            ),
            (
                "a pattern rooted in an _ directory matches beneath it",
                "go test ./_old/...",
                vec![named("_old/old_test.go", "TestOld")],
                commands(&["go test -run '^(TestOld)$' ./_old"]),
            ),
            (
                "a command that is not go test is not narrowed",
                "go vet ./...",
                every.clone(),
                None,
            ),
        ];
        // A one-liner that sets its environment out of sight (`export`,
        // `source`) may build files `go list` cannot see building.
        let hidden: Vec<(&str, &str, Vec<RelatedTest>, Option<FilesListPlan>)> = vec![
            (
                "hidden environment: even a selection that all builds runs as written",
                "go test ./...",
                vec![named("plain/plain_test.go", "TestPlain")],
                Some(FilesListPlan::Full),
            ),
            (
                "hidden environment: a selection go list would cut runs as written",
                "go test ./...",
                vec![
                    named("plain/plain_test.go", "TestPlain"),
                    named("devonly/main_test.go", "TestDev"),
                ],
                Some(FilesListPlan::Full),
            ),
        ];
        for command in [
            "go test -tags \"$DEV_TAGS\" ./...",
            "GOFLAGS=-tags=$DEV_TAGS go test ./...",
        ] {
            assert_eq!(
                GoPlugin.related_tests(tmp.path(), command, &every),
                Some(FilesListPlan::Full),
                "dynamic build context: {command}"
            );
        }
        let mut failures = Vec::new();
        let runs = rows
            .into_iter()
            .map(|row| (false, row))
            .chain(hidden.into_iter().map(|row| (true, row)));
        for (hidden, (case, command, tests, expected)) in runs {
            let actual = if hidden {
                GoPlugin.related_tests_in_hidden_environment(tmp.path(), command, &tests)
            } else {
                GoPlugin.related_tests(tmp.path(), command, &tests)
            };
            if actual != expected {
                failures.push(format!(
                    "{case}: `{command}`\n  expected {expected:?}\n  got      {actual:?}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
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
