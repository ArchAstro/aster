//! Node.js language plugin for package.json parsing

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{
    LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetCapability, TargetContext,
};

/// Internal representation of package.json for serde deserialization
#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<HashMap<String, String>>,
    scripts: Option<HashMap<String, String>>,
    workspaces: Option<WorkspacesConfig>,
    #[serde(rename = "packageManager")]
    package_manager: Option<String>,
}

/// Package manager used by a Node.js project.
///
/// Aster supports npm and pnpm; the enum is the single source of truth for the
/// command strings emitted by `detect_targets`. yarn is intentionally deferred
/// (its `exec` equivalent differs between classic and berry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageManager {
    Npm,
    Pnpm,
}

impl PackageManager {
    /// The CLI binary name.
    fn binary(self) -> &'static str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Pnpm => "pnpm",
        }
    }

    /// Command to install all dependencies.
    fn install(self) -> String {
        format!("{} install", self.binary())
    }

    /// Command to run the conventional `test` script.
    /// Both npm and pnpm accept `<pm> test` as shorthand for `<pm> run test`.
    fn test(self) -> String {
        format!("{} test", self.binary())
    }

    /// Command to run a named script (`<pm> run <script>`).
    fn run(self, script: &str) -> String {
        format!("{} run {script}", self.binary())
    }

    /// Command to execute a locally-installed binary (the `npx` equivalent).
    fn exec(self, cmd: &str) -> String {
        match self {
            PackageManager::Npm => format!("npx {cmd}"),
            PackageManager::Pnpm => format!("pnpm exec {cmd}"),
        }
    }

    /// Name of the exclusive-resource lock used to serialize installs.
    /// npm and pnpm use distinct global caches/stores, so keying the lock on the
    /// binary keeps their installs from being needlessly serialized against each
    /// other while still serializing same-manager installs.
    fn cache_resource(self) -> String {
        format!("{}_cache", self.binary())
    }

    /// Parse the `packageManager` field value (Corepack convention, e.g.
    /// `"pnpm@9.1.0"`). Returns None for unrecognized managers (yarn, bun, …) so
    /// detection can fall through to lockfiles/default.
    fn from_package_manager_field(value: &str) -> Option<Self> {
        match value.split('@').next().unwrap_or(value) {
            "npm" => Some(PackageManager::Npm),
            "pnpm" => Some(PackageManager::Pnpm),
            _ => None,
        }
    }
}

/// Detect which package manager a Node.js project uses.
///
/// Walks up from `project_dir` to `workspace_root` (inclusive). At each level,
/// closest-wins:
///   1. a `packageManager` field in that level's package.json (Corepack convention)
///   2. else a lockfile: `pnpm-lock.yaml` → pnpm, `package-lock.json` → npm
///
/// Walking up means workspace members inherit the root's choice (members share
/// the root lockfile). Defaults to npm when nothing matches.
fn detect_package_manager(project_dir: &Path, workspace_root: &Path) -> PackageManager {
    let mut dir = project_dir;
    loop {
        // 1. packageManager field (only honored if it names a manager we support)
        if let Ok(content) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
                if let Some(pm) = pkg
                    .package_manager
                    .as_deref()
                    .and_then(PackageManager::from_package_manager_field)
                {
                    return pm;
                }
            }
        }

        // 2. lockfiles
        if dir.join("pnpm-lock.yaml").exists() {
            return PackageManager::Pnpm;
        }
        if dir.join("package-lock.json").exists() {
            return PackageManager::Npm;
        }

        if dir == workspace_root {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    PackageManager::Npm
}

/// npm workspaces can be either an array of glob patterns or an object with a `packages` field
#[derive(Deserialize)]
#[serde(untagged)]
enum WorkspacesConfig {
    /// Simple array: `"workspaces": ["packages/*"]`
    Patterns(Vec<String>),
    /// Object form: `"workspaces": { "packages": ["packages/*"] }`
    Object { packages: Vec<String> },
}

impl WorkspacesConfig {
    fn patterns(&self) -> &[String] {
        match self {
            WorkspacesConfig::Patterns(p) => p,
            WorkspacesConfig::Object { packages } => packages,
        }
    }
}

/// Node.js plugin for discovering and parsing npm/yarn/pnpm projects
pub struct NodeJsPlugin;

impl LanguagePlugin for NodeJsPlugin {
    fn name(&self) -> &str {
        "nodejs"
    }

    fn marker_files(&self) -> &[&str] {
        &["package.json"]
    }

    fn should_skip(&self, config_path: &Path) -> bool {
        let Some(project_dir) = config_path.parent() else {
            return false;
        };
        // Rails applications and packaged gems often colocate JavaScript
        // assets with their primary Ruby project. Aster addresses are
        // directory-based, so prefer the Ruby build when a gemspec or Rails
        // executable makes that ownership unambiguous.
        (project_dir.join("Gemfile").is_file() && project_dir.join("bin/rails").is_file())
            || std::fs::read_dir(project_dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "gemspec")
                })
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pkg: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        // Use the package name if present, otherwise fall back to the directory name.
        // Private workspace roots (e.g. monorepo root package.json) commonly omit "name".
        let name = pkg
            .name
            .filter(|n| !n.is_empty())
            .or_else(|| {
                config_path
                    .parent()
                    .and_then(|d| d.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| {
                anyhow!(
                    "Cannot determine project name for {}",
                    config_path.display()
                )
            })?;

        Ok(ProjectMetadata {
            name,
            version: pkg.version,
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pkg: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let project_dir = config_path
            .parent()
            .ok_or_else(|| anyhow!("Config path has no parent directory"))?;

        // Map of package name -> member directory for every workspace sibling,
        // drawn from npm `workspaces` and/or `pnpm-workspace.yaml`. Used to resolve
        // dependencies declared by name (the `workspace:*` protocol, plain-semver
        // siblings, and `workspace:` aliases).
        let workspace_members = resolve_workspace_members(config_path);

        let mut deps = Vec::new();
        // Dedupe by resolved path so a package listed in both `dependencies` and
        // `devDependencies` (or via an alias and directly) yields a single edge.
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();

        for dep_map in [&pkg.dependencies, &pkg.dev_dependencies]
            .into_iter()
            .flatten()
        {
            for (name, version) in dep_map {
                // Resolve this specifier to a member directory, if it denotes a
                // local inter-project dependency.
                let resolved = if let Some(path_str) = version.strip_prefix("file:") {
                    // file: local path dep — resolve relative to this project.
                    Some(project_dir.join(path_str))
                } else if let Some(rest) = version.strip_prefix("workspace:") {
                    // pnpm/yarn(berry)/bun `workspace:` protocol.
                    match parse_workspace_specifier(rest, name) {
                        WorkspaceTarget::Path(rel) => Some(project_dir.join(rel)),
                        WorkspaceTarget::Name(target) => workspace_members.get(target).cloned(),
                    }
                } else {
                    // Bare sibling reference: the dep key matches a workspace
                    // member (e.g. npm workspaces declaring `"^1.0.0"`).
                    workspace_members.get(name).cloned()
                };

                if let Some(path) = resolved {
                    if seen_paths.insert(path.clone()) {
                        deps.push(LocalDependency {
                            name: name.clone(),
                            path,
                        });
                    }
                }
            }
        }

        Ok(deps)
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let content = std::fs::read_to_string(ctx.config_path)
            .with_context(|| format!("Failed to read {}", ctx.config_path.display()))?;

        let pkg: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", ctx.config_path.display()))?;

        let mut targets = HashMap::new();

        // Determine which package manager this project uses (npm or pnpm).
        // All generated commands below route through it.
        let pm = detect_package_manager(ctx.project_dir, ctx.workspace_root);

        // Check if this project is a member of an npm workspace.
        // Workspace members share a single node_modules/ at the workspace root,
        // so we skip the per-package deps target entirely and point all dependency
        // references directly at the workspace root's deps target.
        let workspace_root_addr = find_npm_workspace_root(ctx);

        // The deps reference that other targets in this project should depend on.
        let deps_ref = if let Some(ref ws_addr) = workspace_root_addr {
            // Workspace member: depend directly on the workspace root's deps
            format!("{ws_addr}:deps")
        } else {
            // Standalone project or workspace root: run npm install as //self:deps
            targets.insert(
                "deps".to_string(),
                Target {
                    command: pm.install(),
                    depends_on: vec![],
                    capabilities: HashSet::new(),
                    files_glob: None,
                    stream: false,
                    cache: None,
                    invalidates_cache: false,
                    working_dir: None,
                    exclusive_resources: vec![pm.cache_resource()],
                },
            );
            "//self:deps".to_string()
        };

        // Resolve dependency paths to project addresses
        let dependency_addresses = resolve_dependency_addresses(ctx);

        // Build dependencies for non-deps targets:
        // - deps_ref (install dependencies first — either our own or workspace root's)
        // - :build for each project dependency (they must be built first)
        let mut base_deps = vec![deps_ref.clone()];
        for dep_addr in &dependency_addresses {
            base_deps.push(format!("{dep_addr}:build"));
        }

        let format_deps = vec![deps_ref.clone(), "//self:build".to_string()];

        if let Some(scripts) = pkg.scripts {
            // Track scripts to skip entirely (e.g., npm placeholder scripts)
            let mut skip_scripts: HashSet<&str> = HashSet::new();

            // Map npm scripts to aster targets
            // Only add targets for scripts that actually exist
            // Skip npm's default "no test specified" placeholder
            if let Some(test_script) = scripts.get("test") {
                let is_placeholder = test_script.contains("no test specified")
                    || test_script.contains("Error: no test");
                if is_placeholder {
                    skip_scripts.insert("test");
                } else {
                    let mut test_caps = HashSet::new();
                    test_caps.insert(TargetCapability::FilesList);
                    targets.insert(
                        "test".to_string(),
                        Target {
                            command: pm.test(),
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
                }
            }
            if scripts.contains_key("build") {
                targets.insert(
                    "build".to_string(),
                    Target {
                        command: pm.run("build"),
                        depends_on: base_deps.clone(),
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
            if scripts.contains_key("lint") {
                targets.insert(
                    "lint".to_string(),
                    Target {
                        command: pm.run("lint"),
                        depends_on: base_deps.clone(),
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
            if scripts.contains_key("format") {
                targets.insert(
                    "format".to_string(),
                    Target {
                        command: pm.run("format"),
                        depends_on: format_deps.clone(),
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
            // Also map any other scripts as targets (with same dependencies)
            for (script_name, _) in scripts {
                if !targets.contains_key(&script_name)
                    && !skip_scripts.contains(script_name.as_str())
                {
                    targets.insert(
                        script_name.clone(),
                        Target {
                            command: pm.run(&script_name),
                            depends_on: base_deps.clone(),
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
            }
        }

        // If no format script, check for Prettier config
        if !targets.contains_key("format") {
            let prettier_configs = [
                ".prettierrc",
                ".prettierrc.json",
                ".prettierrc.js",
                ".prettierrc.cjs",
                ".prettierrc.mjs",
                ".prettierrc.yml",
                ".prettierrc.yaml",
                ".prettierrc.toml",
                "prettier.config.js",
                "prettier.config.cjs",
                "prettier.config.mjs",
            ];
            let has_prettier = prettier_configs
                .iter()
                .any(|config| ctx.project_dir.join(config).exists());

            if has_prettier {
                targets.insert(
                    "format".to_string(),
                    Target {
                        command: pm.exec("prettier --write ."),
                        depends_on: format_deps,
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
        }

        // Add clean target
        if let Some(clean) = self.clean_target(ctx) {
            targets.insert("clean".to_string(), clean);
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

        // Filter to test files only (*.test.*, *.spec.*, __tests__/*)
        let test_files: Vec<&PathBuf> = files
            .iter()
            .filter(|f| {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let path_str = f.to_string_lossy();
                name.contains(".test.")
                    || name.contains(".spec.")
                    || name.contains("_test.")
                    || path_str.contains("__tests__")
                    || path_str.contains("tests/")
            })
            .collect();

        if test_files.is_empty() {
            // No test files in the change set - run full test suite
            return None;
        }

        // npm test -- file1 file2 (-- passes args through to the underlying test runner)
        let file_args: Vec<String> = test_files
            .iter()
            .map(|f| crate::executor::quote_command_argument(&f.to_string_lossy()))
            .collect();

        Some(format!("{} -- {}", command, file_args.join(" ")))
    }

    fn cache_inputs(&self, target_name: &str) -> super::CacheInputs {
        // deps target only cares about dependency specification files,
        // not source code — source changes shouldn't re-install dependencies
        if target_name == "deps" {
            return super::CacheInputs {
                source_globs: vec![],
                config_files: vec![
                    "package.json".to_string(),
                    "package-lock.json".to_string(),
                    "yarn.lock".to_string(),
                    "pnpm-lock.yaml".to_string(),
                ],
                env_vars: vec!["NODE_ENV".to_string()],
            };
        }

        let mut inputs = super::CacheInputs {
            source_globs: vec![
                "src/**/*.ts".to_string(),
                "src/**/*.tsx".to_string(),
                "src/**/*.js".to_string(),
                "src/**/*.jsx".to_string(),
                "lib/**/*.ts".to_string(),
                "lib/**/*.js".to_string(),
            ],
            config_files: vec![
                "package.json".to_string(),
                "package-lock.json".to_string(),
                "yarn.lock".to_string(),
                "pnpm-lock.yaml".to_string(),
                "tsconfig.json".to_string(),
            ],
            env_vars: vec!["NODE_ENV".to_string(), "CI".to_string()],
        };

        // Add test files for test target
        if target_name == "test" {
            inputs.source_globs.push("test/**/*.ts".to_string());
            inputs.source_globs.push("test/**/*.js".to_string());
            inputs.source_globs.push("__tests__/**/*.ts".to_string());
            inputs.source_globs.push("__tests__/**/*.js".to_string());
            inputs.source_globs.push("**/*.test.ts".to_string());
            inputs.source_globs.push("**/*.spec.ts".to_string());
        }

        inputs
    }

    fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
        let mut dirs_to_clean = vec!["node_modules"];

        // Detect framework-specific directories
        if ctx.project_dir.join(".next").exists() {
            dirs_to_clean.push(".next");
        }
        if ctx.project_dir.join("dist").exists() {
            dirs_to_clean.push("dist");
        }
        if ctx.project_dir.join(".turbo").exists() {
            dirs_to_clean.push(".turbo");
        }
        if ctx.project_dir.join("build").exists() {
            dirs_to_clean.push("build");
        }

        let arguments = dirs_to_clean
            .iter()
            .map(|path| crate::executor::quote_command_argument(path))
            .collect::<Vec<_>>()
            .join(" ");
        let command = format!("rm -rf {arguments}");

        Some(Target {
            command,
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

/// A resolved target for a `workspace:` protocol specifier (the value after the
/// `workspace:` prefix).
enum WorkspaceTarget<'a> {
    /// Path form (`workspace:./pkg`, `workspace:../pkg`): resolve by relative path,
    /// exactly like a `file:` dep.
    Path(&'a str),
    /// Resolve by package name: the dependency key for the plain range forms
    /// (`workspace:*`, `workspace:^`, `workspace:~`, `workspace:1.2.3`, …), or the
    /// aliased target name for the alias form (`workspace:@scope/dep@*`).
    Name(&'a str),
}

/// Parse the portion of a `workspace:` specifier that follows the `workspace:`
/// prefix into the workspace member it points at.
///
/// `dep_key` is the dependency's own package-name key; it's the resolution target
/// for the range forms (`*`, `^`, `~`, `1.2.3`, …) that don't name a package.
fn parse_workspace_specifier<'a>(rest: &'a str, dep_key: &'a str) -> WorkspaceTarget<'a> {
    // Path form: workspace:./path or workspace:../path
    if rest.starts_with("./") || rest.starts_with("../") {
        return WorkspaceTarget::Path(rest);
    }

    // Alias form: workspace:<targetName>@<range>, where <targetName> may itself be
    // scoped (e.g. @scope/dep). The delimiter is the `@` separating name from
    // range — i.e. any `@` other than a leading scope marker. Ranges never
    // contain `@`, so its presence reliably signals an alias.
    let scope_offset = usize::from(rest.starts_with('@'));
    if let Some(rel) = rest[scope_offset..].find('@') {
        let at = scope_offset + rel;
        let name = &rest[..at];
        if !name.is_empty() {
            return WorkspaceTarget::Name(name);
        }
    }

    // Range form: resolve by the dependency key.
    WorkspaceTarget::Name(dep_key)
}

/// Resolve all workspace members reachable from a package.json's location.
///
/// Walks up from `config_path` to the nearest workspace root — either a parent
/// `package.json` with a `workspaces` field (npm/yarn classic) or a
/// `pnpm-workspace.yaml` (pnpm/yarn-berry/bun) — then expands the workspace glob
/// patterns to build a map of package name → absolute filesystem path for every
/// workspace member.
///
/// Returns an empty map if the package is not inside a workspace.
fn resolve_workspace_members(config_path: &Path) -> HashMap<String, PathBuf> {
    let mut members = HashMap::new();

    let project_dir = match config_path.parent() {
        Some(d) => d,
        None => return members,
    };

    // Walk up from project_dir looking for the nearest workspace root.
    let mut candidate = project_dir.to_path_buf();
    loop {
        candidate = match candidate.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };

        let patterns = workspace_patterns_at(&candidate);
        if patterns.is_empty() {
            continue;
        }

        // Found a workspace root — expand patterns to discover members.
        for pattern in &patterns {
            // Skip pnpm exclusion patterns (e.g. "!packages/excluded").
            if pattern.starts_with('!') {
                continue;
            }
            expand_workspace_pattern(&candidate, pattern, &mut members);
        }

        // Only use the nearest workspace root.
        break;
    }

    members
}

/// Read the workspace glob patterns declared at `dir`, if it is a workspace root.
///
/// Prefers a `package.json` `workspaces` field (npm/yarn classic); otherwise falls
/// back to `pnpm-workspace.yaml` (pnpm/yarn-berry/bun). Returns an empty vec when
/// `dir` is not a workspace root.
fn workspace_patterns_at(dir: &Path) -> Vec<String> {
    // npm/yarn classic: package.json "workspaces"
    if let Ok(content) = std::fs::read_to_string(dir.join("package.json")) {
        if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
            if let Some(ws) = pkg.workspaces {
                return ws.patterns().to_vec();
            }
        }
    }

    // pnpm/yarn-berry/bun: pnpm-workspace.yaml "packages:"
    if let Ok(content) = std::fs::read_to_string(dir.join("pnpm-workspace.yaml")) {
        return parse_pnpm_workspace_packages(&content);
    }

    Vec::new()
}

/// Extract the `packages:` glob list from a `pnpm-workspace.yaml`.
///
/// Supports both the block-list form and a simple inline flow sequence:
///
/// ```yaml
/// packages:
///   - 'packages/*'
///   - "apps/*"
/// ```
///
/// ```yaml
/// packages: ['packages/*', 'apps/*']
/// ```
///
/// This is a deliberately minimal parser (the project has no YAML dependency); it
/// reads just the one field aster needs and ignores everything else.
fn parse_pnpm_workspace_packages(content: &str) -> Vec<String> {
    let mut packages = Vec::new();
    let mut in_block = false;

    for raw_line in content.lines() {
        // Drop trailing comments and surrounding whitespace.
        let line = raw_line.split('#').next().unwrap_or(raw_line);
        if line.trim().is_empty() {
            continue;
        }

        if !in_block {
            let stripped = match line.trim_start().strip_prefix("packages:") {
                Some(rest) => rest.trim(),
                None => continue,
            };

            if stripped.is_empty() {
                // Block list follows on subsequent lines.
                in_block = true;
            } else if let Some(inner) = stripped.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
            {
                // Inline flow sequence: packages: ['a', 'b']
                for item in inner.split(',') {
                    let item = unquote_yaml_scalar(item.trim());
                    if !item.is_empty() {
                        packages.push(item.to_string());
                    }
                }
                break;
            }
            continue;
        }

        // Inside the block list: collect `- <pattern>` entries.
        match line.trim_start().strip_prefix('-') {
            Some(item) => {
                let item = unquote_yaml_scalar(item.trim());
                if !item.is_empty() {
                    packages.push(item.to_string());
                }
            }
            // A non-list line ends the `packages:` block (e.g. another top-level key).
            None => break,
        }
    }

    packages
}

/// Strip a single layer of matching single or double quotes from a YAML scalar.
fn unquote_yaml_scalar(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Expand a single npm workspace glob pattern (e.g. "packages/*") relative to the workspace root,
/// reading each matched directory's package.json to extract the package name.
fn expand_workspace_pattern(
    workspace_root: &Path,
    pattern: &str,
    members: &mut HashMap<String, PathBuf>,
) {
    // Split pattern into directory prefix and optional glob segment
    // e.g. "packages/*" -> parent="packages", has glob
    // e.g. "platform-sdk" -> exact path, no glob
    let parts: Vec<&str> = pattern.split('/').collect();

    // Find the first segment containing '*'
    let glob_index = parts.iter().position(|p| p.contains('*'));

    match glob_index {
        Some(idx) => {
            // Build the parent directory path from segments before the glob
            let parent_dir: PathBuf = parts[..idx]
                .iter()
                .fold(workspace_root.to_path_buf(), |acc, seg| acc.join(seg));

            let glob_segment = parts[idx];

            // Read entries in the parent directory
            let entries = match std::fs::read_dir(&parent_dir) {
                Ok(e) => e,
                Err(_) => return,
            };

            for entry in entries.flatten() {
                let entry_path = entry.path();
                if !entry_path.is_dir() {
                    continue;
                }

                let dir_name = match entry_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };

                // Match against the glob segment (simple * matching)
                if glob_segment == "*" || dir_name == glob_segment {
                    // If there are more segments after the glob, we'd need recursion,
                    // but npm workspace patterns are typically one level deep
                    let member_dir = if idx + 1 < parts.len() {
                        parts[idx + 1..]
                            .iter()
                            .fold(entry_path.clone(), |acc, seg| acc.join(seg))
                    } else {
                        entry_path.clone()
                    };

                    try_add_workspace_member(&member_dir, members);
                }
            }
        }
        None => {
            // Exact path — no glob
            let member_dir = workspace_root.join(pattern);
            try_add_workspace_member(&member_dir, members);
        }
    }
}

/// Read a directory's package.json and add it to the workspace members map if it has a name.
fn try_add_workspace_member(dir: &Path, members: &mut HashMap<String, PathBuf>) {
    let pkg_path = dir.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_path) {
        if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
            if let Some(name) = pkg.name {
                if !name.is_empty() {
                    members.insert(name, dir.to_path_buf());
                }
            }
        }
    }
}

/// Detect if this project is a member of an npm workspace.
///
/// Walks up from `project_dir` toward `workspace_root` looking for a parent
/// `package.json` with a `workspaces` field whose glob patterns include this project.
///
/// Returns the aster project address of the npm workspace root (e.g., `//src/ts/native-templates`)
/// if this project is a workspace member, or None if it's standalone.
fn find_npm_workspace_root(ctx: &TargetContext) -> Option<String> {
    let project_dir = ctx.project_dir;

    // Walk up from project_dir, stopping before workspace_root
    let mut candidate = project_dir.parent()?;
    while candidate.starts_with(ctx.workspace_root) && candidate != ctx.workspace_root {
        let parent_pkg = candidate.join("package.json");
        if parent_pkg.exists() {
            if let Ok(content) = std::fs::read_to_string(&parent_pkg) {
                if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
                    if let Some(ref ws) = pkg.workspaces {
                        // Check if our project matches one of the workspace patterns
                        let relative = project_dir.strip_prefix(candidate).ok()?;
                        let relative_str = relative.to_string_lossy();

                        for pattern in ws.patterns() {
                            if matches_workspace_pattern(pattern, &relative_str) {
                                // Return aster address of the npm workspace root
                                let ws_relative =
                                    candidate.strip_prefix(ctx.workspace_root).ok()?;
                                return Some(format!("//{}", ws_relative.display()));
                            }
                        }
                    }
                }
            }
        }
        candidate = candidate.parent()?;
    }

    // Also check the workspace root itself
    let root_pkg = ctx.workspace_root.join("package.json");
    if root_pkg.exists() && ctx.project_dir != ctx.workspace_root {
        if let Ok(content) = std::fs::read_to_string(&root_pkg) {
            if let Ok(pkg) = serde_json::from_str::<PackageJson>(&content) {
                if let Some(ref ws) = pkg.workspaces {
                    let relative = project_dir.strip_prefix(ctx.workspace_root).ok()?;
                    let relative_str = relative.to_string_lossy();

                    for pattern in ws.patterns() {
                        if matches_workspace_pattern(pattern, &relative_str) {
                            // Workspace root is at the aster workspace root
                            return Some("//".to_string());
                        }
                    }
                }
            }
        }
    }

    None
}

/// Check if a relative path matches an npm workspace glob pattern.
///
/// npm workspace patterns use simple globs: `packages/*`, `apps/*`, or exact paths.
/// `*` matches a single directory segment (not recursive).
fn matches_workspace_pattern(pattern: &str, relative_path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = relative_path.split('/').filter(|s| !s.is_empty()).collect();

    if pattern_parts.len() != path_parts.len() {
        return false;
    }

    pattern_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(pat, path)| *pat == "*" || *pat == *path)
}

/// Resolve LocalDependency paths to project addresses
///
/// Handles both address strings (//libs/shared) and relative paths (../../libs/shared)
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
    fn test_parse_simple_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app", "version": "1.2.3"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let metadata = plugin.parse_project(tmp.path(), &pkg_json).unwrap();

        assert_eq!(metadata.name, "my-app");
        assert_eq!(metadata.version, Some("1.2.3".to_string()));
    }

    #[test]
    fn test_parse_file_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
                "name": "my-app",
                "dependencies": {
                    "shared": "file:../shared"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&pkg_json).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "shared");
        assert_eq!(deps[0].path, tmp.path().join("../shared"));
    }

    #[test]
    fn test_parse_file_dev_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
                "name": "my-app",
                "devDependencies": {
                    "test-utils": "file:../test-utils"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&pkg_json).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "test-utils");
        assert_eq!(deps[0].path, tmp.path().join("../test-utils"));
    }

    #[test]
    fn test_parse_mixed_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{
                "name": "my-app",
                "dependencies": {
                    "lodash": "^4.17.21",
                    "local-lib": "file:../local-lib",
                    "react": "^18.0.0"
                },
                "devDependencies": {
                    "typescript": "^5.0.0",
                    "local-types": "file:../local-types"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&pkg_json).unwrap();

        // Only file: dependencies should be extracted
        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"local-lib"));
        assert!(names.contains(&"local-types"));
    }

    #[test]
    fn test_parse_missing_name_falls_back_to_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"version": "1.0.0"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let result = plugin.parse_project(tmp.path(), &pkg_json).unwrap();

        // Falls back to the directory name from tempdir
        assert!(!result.name.is_empty());
    }

    #[test]
    fn test_parse_empty_name_falls_back_to_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "", "version": "1.0.0"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let result = plugin.parse_project(tmp.path(), &pkg_json).unwrap();

        // Falls back to the directory name from tempdir
        assert!(!result.name.is_empty());
    }

    #[test]
    fn test_parse_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{ invalid json }"#).unwrap();

        let plugin = NodeJsPlugin;
        let result = plugin.parse_project(tmp.path(), &pkg_json);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
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
    fn test_detect_targets_with_all_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest", "build": "tsc", "lint": "eslint ."}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Check commands
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"npm run build".to_string())
        );
        assert_eq!(
            targets.get("lint").map(|t| &t.command),
            Some(&"npm run lint".to_string())
        );

        // Check that deps target exists
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );

        // Check dependencies (//self:deps)
        assert_eq!(targets.get("test").unwrap().depends_on, vec!["//self:deps"]);
        assert_eq!(
            targets.get("build").unwrap().depends_on,
            vec!["//self:deps"]
        );
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
    }

    #[test]
    fn test_detect_targets_with_project_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize workspace root to handle macOS /var -> /private/var symlink
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create workspace structure
        let project_dir = workspace_root.join("apps/web");
        std::fs::create_dir_all(&project_dir).unwrap();
        let pkg_json = project_dir.join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest", "build": "tsc"}}"#,
        )
        .unwrap();

        // Create dependency directories (needed for canonicalize)
        let core_dir = workspace_root.join("libs/core");
        let utils_dir = workspace_root.join("libs/utils");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::create_dir_all(&utils_dir).unwrap();

        // Dependencies with relative paths
        let dependencies = vec![
            LocalDependency {
                name: "core".to_string(),
                path: "../../libs/core".into(),
            },
            LocalDependency {
                name: "utils".to_string(),
                path: "../../libs/utils".into(),
            },
        ];

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, &workspace_root, &dependencies);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test should depend on deps and build of each dependency
        let test_deps = &targets.get("test").unwrap().depends_on;
        assert!(test_deps.contains(&"//self:deps".to_string()));
        assert!(test_deps.contains(&"//libs/core:build".to_string()));
        assert!(test_deps.contains(&"//libs/utils:build".to_string()));

        // build should also depend on deps and build of each dependency
        let build_deps = &targets.get("build").unwrap().depends_on;
        assert!(build_deps.contains(&"//self:deps".to_string()));
        assert!(build_deps.contains(&"//libs/core:build".to_string()));
        assert!(build_deps.contains(&"//libs/utils:build".to_string()));

        // deps should NOT depend on project dependencies
        assert!(targets.get("deps").unwrap().depends_on.is_empty());
    }

    #[test]
    fn test_detect_targets_with_some_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test and deps are present
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"npm test".to_string())
        );
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );
        // build and lint are not present (no scripts for them)
        assert_eq!(targets.get("build"), None);
        assert_eq!(targets.get("lint"), None);
    }

    #[test]
    fn test_detect_targets_no_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app", "version": "1.0.0"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // deps and clean targets are always present
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"npm install".to_string())
        );
        assert!(targets.contains_key("clean"));
    }

    #[test]
    fn test_detect_targets_custom_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"deploy": "npm publish", "validate": "npm run test && npm run lint"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Custom scripts are mapped (with deps dependency)
        assert_eq!(
            targets.get("deploy").map(|t| &t.command),
            Some(&"npm run deploy".to_string())
        );
        assert_eq!(
            targets.get("validate").map(|t| &t.command),
            Some(&"npm run validate".to_string())
        );
        assert_eq!(
            targets.get("deploy").unwrap().depends_on,
            vec!["//self:deps"]
        );
        // Standard targets not present (not in scripts)
        assert_eq!(targets.get("test"), None);
        assert_eq!(targets.get("build"), None);
        // deps is always present
        assert!(targets.contains_key("deps"));
    }

    #[test]
    fn test_detect_targets_format_script() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"format": "prettier --write ."}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses npm run format
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npm run format".to_string())
        );

        // Check dependencies
        let format_deps = &targets.get("format").unwrap().depends_on;
        assert!(format_deps.contains(&"//self:deps".to_string()));
        assert!(format_deps.contains(&"//self:build".to_string()));
        assert_eq!(format_deps.len(), 2);
    }

    #[test]
    fn test_detect_targets_format_prettier_config() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        // Create .prettierrc
        std::fs::write(tmp.path().join(".prettierrc"), r#"{"semi": false}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses npx prettier
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npx prettier --write .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_format_prettier_config_js() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        // Create prettier.config.js
        std::fs::write(
            tmp.path().join("prettier.config.js"),
            "module.exports = { semi: false };",
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format uses npx prettier
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npx prettier --write .".to_string())
        );
    }

    #[test]
    fn test_detect_targets_format_script_preferred_over_prettier() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"format": "biome format --write ."}}"#,
        )
        .unwrap();

        // Also create .prettierrc
        std::fs::write(tmp.path().join(".prettierrc"), r#"{"semi": false}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // format script takes priority over prettier config
        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"npm run format".to_string())
        );
    }

    #[test]
    fn test_detect_targets_no_format_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // no format target (no script or prettier config)
        assert_eq!(targets.get("format"), None);
    }

    #[test]
    fn test_with_files_list_filters_test_files() {
        let plugin = NodeJsPlugin;
        let files = vec![
            PathBuf::from("src/index.ts"),
            PathBuf::from("src/utils.test.ts"),
            PathBuf::from("src/helpers.spec.js"),
            PathBuf::from("__tests__/integration.js"),
            PathBuf::from("package.json"),
        ];

        let result = plugin.with_files_list("test", "npm test", &files);

        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.starts_with("npm test -- "));
        assert!(cmd.contains("utils.test.ts"));
        assert!(cmd.contains("helpers.spec.js"));
        assert!(cmd.contains("__tests__/integration.js"));
        assert!(!cmd.contains("index.ts"));
        assert!(!cmd.contains("package.json"));
    }

    #[test]
    fn test_with_files_list_returns_none_for_non_test_target() {
        let plugin = NodeJsPlugin;
        let files = vec![PathBuf::from("src/index.test.ts")];

        let result = plugin.with_files_list("build", "npm run build", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_returns_none_when_no_test_files() {
        let plugin = NodeJsPlugin;
        let files = vec![
            PathBuf::from("src/index.ts"),
            PathBuf::from("src/utils.ts"),
            PathBuf::from("package.json"),
        ];

        let result = plugin.with_files_list("test", "npm test", &files);

        // No test files, so run full suite
        assert!(result.is_none());
    }

    #[test]
    fn test_with_files_list_empty_files() {
        let plugin = NodeJsPlugin;
        let files: Vec<PathBuf> = vec![];

        let result = plugin.with_files_list("test", "npm test", &files);

        assert!(result.is_none());
    }

    #[test]
    fn test_test_target_has_files_list_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "jest"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let test_target = targets.get("test").unwrap();
        assert!(test_target
            .capabilities
            .contains(&TargetCapability::FilesList));

        // build should not have the capability
        let deps_target = targets.get("deps").unwrap();
        assert!(!deps_target
            .capabilities
            .contains(&TargetCapability::FilesList));
    }

    #[test]
    fn test_skip_npm_placeholder_test_script() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        // This is the default npm init test script that should be ignored
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test target should NOT be created for placeholder script
        assert_eq!(targets.get("test"), None);

        // deps is always present
        assert!(targets.contains_key("deps"));
    }

    #[test]
    fn test_skip_npm_placeholder_test_script_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        // Another variant of the placeholder
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"test": "echo 'Error: no test' && exit 1"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // test target should NOT be created for placeholder script
        assert_eq!(targets.get("test"), None);
    }

    #[test]
    fn test_detect_targets_has_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert_eq!(clean.command, "rm -rf node_modules");
        assert!(clean.depends_on.is_empty());
        assert!(clean.invalidates_cache);
    }

    #[test]
    fn test_clean_target_detects_next() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

        // Create .next directory
        std::fs::create_dir(tmp.path().join(".next")).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        let clean = targets.get("clean").unwrap();
        assert!(clean.command.contains("node_modules"));
        assert!(clean.command.contains(".next"));
    }

    #[test]
    fn test_matches_workspace_pattern_glob() {
        assert!(matches_workspace_pattern("packages/*", "packages/core"));
        assert!(matches_workspace_pattern("packages/*", "packages/react"));
        assert!(!matches_workspace_pattern("packages/*", "apps/web"));
        assert!(!matches_workspace_pattern(
            "packages/*",
            "packages/deep/nested"
        ));
    }

    #[test]
    fn test_matches_workspace_pattern_exact() {
        assert!(matches_workspace_pattern(
            "apps/storybook-react",
            "apps/storybook-react"
        ));
        assert!(!matches_workspace_pattern(
            "apps/storybook-react",
            "apps/web"
        ));
    }

    #[test]
    fn test_workspace_member_has_no_deps_target() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root with workspaces field
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member
        let member_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/core", "scripts": {"build": "tsc", "test": "vitest"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&member_pkg, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // No deps target — workspace root handles installation
        assert!(!targets.contains_key("deps"));

        // Other targets depend directly on the workspace root's deps
        let build = targets.get("build").unwrap();
        assert!(build.depends_on.contains(&"//:deps".to_string()));
        let test = targets.get("test").unwrap();
        assert!(test.depends_on.contains(&"//:deps".to_string()));
    }

    #[test]
    fn test_workspace_root_runs_npm_install() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root
        let pkg_json = workspace_root.join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "monorepo", "workspaces": ["packages/*"], "scripts": {"build": "npm run build --workspaces"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Workspace root itself should run npm install directly
        let deps = targets.get("deps").unwrap();
        assert_eq!(deps.command, "npm install");
        assert!(deps.depends_on.is_empty());
    }

    #[test]
    fn test_workspace_member_nested_root() {
        let tmp = tempfile::tempdir().unwrap();
        let aster_root = tmp.path().canonicalize().unwrap();

        // npm workspace root is nested under aster workspace root
        let npm_root = aster_root.join("src/ts/native-templates");
        std::fs::create_dir_all(&npm_root).unwrap();
        std::fs::write(
            npm_root.join("package.json"),
            r#"{"name": "native-templates", "workspaces": ["packages/*", "apps/storybook-react"]}"#,
        )
        .unwrap();

        // Create workspace member
        let member_dir = npm_root.join("packages/react");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/react", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&member_pkg, &aster_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // No deps target on the member
        assert!(!targets.contains_key("deps"));

        // build depends directly on the nested npm workspace root's deps
        let build = targets.get("build").unwrap();
        assert!(build
            .depends_on
            .contains(&"//src/ts/native-templates:deps".to_string()));
    }

    #[test]
    fn test_non_workspace_member_not_affected() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root with specific patterns
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create a project NOT matching workspace patterns
        let standalone_dir = workspace_root.join("tools/cli");
        std::fs::create_dir_all(&standalone_dir).unwrap();
        let standalone_pkg = standalone_dir.join("package.json");
        std::fs::write(
            &standalone_pkg,
            r#"{"name": "cli-tool", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&standalone_pkg, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // Should run its own npm install since it's not a workspace member
        let deps = targets.get("deps").unwrap();
        assert_eq!(deps.command, "npm install");
        assert!(deps.depends_on.is_empty());
    }

    #[test]
    fn test_workspace_object_form() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // npm also supports object form: { "packages": [...] }
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": {"packages": ["packages/*"]}}"#,
        )
        .unwrap();

        let member_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/core", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&member_pkg, &workspace_root, &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // No deps target on the member
        assert!(!targets.contains_key("deps"));

        // build depends directly on workspace root's deps
        let build = targets.get("build").unwrap();
        assert!(build.depends_on.contains(&"//:deps".to_string()));
    }

    #[test]
    fn test_parse_workspace_sibling_deps_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member: packages/core
        let core_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(
            core_dir.join("package.json"),
            r#"{"name": "@archastro/core", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Create workspace member: packages/sdk that depends on core by name
        let sdk_dir = workspace_root.join("packages/sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let sdk_pkg = sdk_dir.join("package.json");
        std::fs::write(
            &sdk_pkg,
            r#"{
                "name": "@archastro/sdk",
                "dependencies": {
                    "@archastro/core": "^1.0.0",
                    "lodash": "^4.17.21"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&sdk_pkg).unwrap();

        // Should detect @archastro/core as a local dependency
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@archastro/core");
        assert_eq!(deps[0].path, core_dir);
    }

    #[test]
    fn test_parse_workspace_sibling_deps_in_dev_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member: packages/test-utils
        let utils_dir = workspace_root.join("packages/test-utils");
        std::fs::create_dir_all(&utils_dir).unwrap();
        std::fs::write(
            utils_dir.join("package.json"),
            r#"{"name": "@archastro/test-utils", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Create workspace member: packages/app with devDependency on test-utils
        let app_dir = workspace_root.join("packages/app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_pkg = app_dir.join("package.json");
        std::fs::write(
            &app_pkg,
            r#"{
                "name": "@archastro/app",
                "devDependencies": {
                    "@archastro/test-utils": "^1.0.0",
                    "typescript": "^5.0.0"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&app_pkg).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@archastro/test-utils");
        assert_eq!(deps[0].path, utils_dir);
    }

    #[test]
    fn test_parse_workspace_sibling_deps_not_duplicated_with_file_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member: packages/core
        let core_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(
            core_dir.join("package.json"),
            r#"{"name": "@archastro/core", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Create workspace member that uses file: prefix (already handled)
        let sdk_dir = workspace_root.join("packages/sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let sdk_pkg = sdk_dir.join("package.json");
        std::fs::write(
            &sdk_pkg,
            r#"{
                "name": "@archastro/sdk",
                "dependencies": {
                    "@archastro/core": "file:../core"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&sdk_pkg).unwrap();

        // Should only have one dep (from file: prefix), not duplicated
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@archastro/core");
        // The file: path is relative
        assert_eq!(deps[0].path, sdk_dir.join("../core"));
    }

    #[test]
    fn test_parse_workspace_sibling_deps_nested_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // npm workspace root is nested
        let npm_root = root.join("src/ts/native-templates");
        std::fs::create_dir_all(&npm_root).unwrap();
        std::fs::write(
            npm_root.join("package.json"),
            r#"{"name": "native-templates", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member: packages/core
        let core_dir = npm_root.join("packages/core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(
            core_dir.join("package.json"),
            r#"{"name": "@archastro/native-templates-core", "version": "1.0.0"}"#,
        )
        .unwrap();

        // Create workspace member: packages/react that depends on core
        let react_dir = npm_root.join("packages/react");
        std::fs::create_dir_all(&react_dir).unwrap();
        let react_pkg = react_dir.join("package.json");
        std::fs::write(
            &react_pkg,
            r#"{
                "name": "@archastro/native-templates-react",
                "dependencies": {
                    "@archastro/native-templates-core": "^1.0.0",
                    "react": "^18.0.0"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&react_pkg).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@archastro/native-templates-core");
        assert_eq!(deps[0].path, core_dir);
    }

    #[test]
    fn test_workspace_sibling_deps_end_to_end_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // Create npm workspace root (also the aster workspace root)
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create workspace member: packages/core
        let core_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(
            core_dir.join("package.json"),
            r#"{"name": "@archastro/core", "version": "1.0.0", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        // Create workspace member: packages/sdk that depends on core by name
        let sdk_dir = workspace_root.join("packages/sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let sdk_pkg = sdk_dir.join("package.json");
        std::fs::write(
            &sdk_pkg,
            r#"{
                "name": "@archastro/sdk",
                "scripts": {"build": "tsc"},
                "dependencies": {
                    "@archastro/core": "^1.0.0"
                }
            }"#,
        )
        .unwrap();

        // Parse dependencies for sdk
        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&sdk_pkg).unwrap();

        // Now detect targets with those dependencies
        let ctx = TargetContext {
            config_path: &sdk_pkg,
            project_dir: &sdk_dir,
            workspace_root: &workspace_root,
            dependencies: &deps,
        };
        let targets = plugin.detect_targets(&ctx).unwrap();

        // sdk:build should depend on core:build
        let build = targets.get("build").unwrap();
        assert!(build
            .depends_on
            .contains(&"//packages/core:build".to_string()));
    }

    #[test]
    fn test_from_package_manager_field() {
        assert_eq!(
            PackageManager::from_package_manager_field("npm"),
            Some(PackageManager::Npm)
        );
        assert_eq!(
            PackageManager::from_package_manager_field("pnpm@9.1.0"),
            Some(PackageManager::Pnpm)
        );
        // Unsupported managers fall through (None) so detection can default.
        assert_eq!(
            PackageManager::from_package_manager_field("yarn@4.0.0"),
            None
        );
        assert_eq!(PackageManager::from_package_manager_field("bun"), None);
    }

    #[test]
    fn test_detect_pnpm_via_package_manager_field() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "packageManager": "pnpm@9.1.0", "scripts": {"test": "vitest", "build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"pnpm install".to_string())
        );
        assert_eq!(
            targets.get("test").map(|t| &t.command),
            Some(&"pnpm test".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"pnpm run build".to_string())
        );
    }

    #[test]
    fn test_detect_pnpm_via_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();
        // No packageManager field — detection falls back to the lockfile.
        std::fs::write(
            tmp.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("deps").map(|t| &t.command),
            Some(&"pnpm install".to_string())
        );
        assert_eq!(
            targets.get("build").map(|t| &t.command),
            Some(&"pnpm run build".to_string())
        );
    }

    #[test]
    fn test_detect_pnpm_install_uses_pnpm_cache_resource() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "packageManager": "pnpm@9.1.0"}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        // pnpm installs serialize on their own resource, not npm's.
        let deps = targets.get("deps").unwrap();
        assert_eq!(deps.exclusive_resources, vec!["pnpm_cache".to_string()]);
    }

    #[test]
    fn test_detect_pnpm_prettier_uses_pnpm_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_json = tmp.path().join("package.json");
        std::fs::write(
            &pkg_json,
            r#"{"name": "my-app", "packageManager": "pnpm@9.1.0"}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join(".prettierrc"), r#"{"semi": false}"#).unwrap();

        let plugin = NodeJsPlugin;
        let ctx = make_context(&pkg_json, tmp.path(), &[]);
        let targets = plugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets.get("format").map(|t| &t.command),
            Some(&"pnpm exec prettier --write .".to_string())
        );
    }

    #[test]
    fn test_workspace_member_inherits_pnpm_from_root() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        // pnpm workspace root: lockfile at the root, members share it.
        std::fs::write(
            workspace_root.join("package.json"),
            r#"{"name": "monorepo", "workspaces": ["packages/*"]}"#,
        )
        .unwrap();
        std::fs::write(
            workspace_root.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();

        // Workspace member (no lockfile of its own).
        let member_dir = workspace_root.join("packages/core");
        std::fs::create_dir_all(&member_dir).unwrap();
        let member_pkg = member_dir.join("package.json");
        std::fs::write(
            &member_pkg,
            r#"{"name": "@scope/core", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;

        // Member infers pnpm by walking up to the root lockfile.
        let member_ctx = make_context(&member_pkg, &workspace_root, &[]);
        let member_targets = plugin.detect_targets(&member_ctx).unwrap();
        assert!(!member_targets.contains_key("deps"));
        assert_eq!(
            member_targets.get("build").map(|t| &t.command),
            Some(&"pnpm run build".to_string())
        );
        assert!(member_targets
            .get("build")
            .unwrap()
            .depends_on
            .contains(&"//:deps".to_string()));

        // Root runs pnpm install.
        let root_pkg = workspace_root.join("package.json");
        let root_ctx = make_context(&root_pkg, &workspace_root, &[]);
        let root_targets = plugin.detect_targets(&root_ctx).unwrap();
        assert_eq!(
            root_targets.get("deps").map(|t| &t.command),
            Some(&"pnpm install".to_string())
        );
    }

    // --- workspace: protocol (pnpm/yarn-berry/bun) ---------------------------

    /// Create a workspace member directory with a buildable package.json.
    fn write_member(root: &Path, rel: &str, name: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name": "{name}", "version": "1.0.0", "scripts": {{"build": "tsc"}}}}"#),
        )
        .unwrap();
        dir
    }

    #[test]
    fn test_parse_pnpm_workspace_packages() {
        // Block list, with quotes, a trailing comment, and a following key.
        let block = "packages:\n  - 'packages/*'\n  - \"apps/*\" # internal\n  - libs/shared\nonlyBuiltDependencies:\n  - esbuild\n";
        assert_eq!(
            parse_pnpm_workspace_packages(block),
            vec!["packages/*", "apps/*", "libs/shared"]
        );

        // Inline flow sequence.
        let inline = "packages: ['packages/*', \"apps/*\"]\n";
        assert_eq!(
            parse_pnpm_workspace_packages(inline),
            vec!["packages/*", "apps/*"]
        );

        // Exclusion patterns are returned verbatim; the caller filters them.
        let excl = "packages:\n  - 'packages/*'\n  - '!packages/excluded'\n";
        assert_eq!(
            parse_pnpm_workspace_packages(excl),
            vec!["packages/*", "!packages/excluded"]
        );

        // A file without a packages: key yields nothing.
        assert!(parse_pnpm_workspace_packages("name: foo\n").is_empty());
    }

    #[test]
    fn test_parse_workspace_specifier_forms() {
        fn target<'a>(spec: &'a str, key: &'a str) -> (Option<&'a str>, Option<&'a str>) {
            match parse_workspace_specifier(spec, key) {
                WorkspaceTarget::Name(n) => (Some(n), None),
                WorkspaceTarget::Path(p) => (None, Some(p)),
            }
        }

        // Range forms resolve by the dependency key.
        for range in ["*", "^", "~", "^1.2.3", "~1.2.3", "1.2.3", ">=1.0.0"] {
            assert_eq!(target(range, "@scope/dep"), (Some("@scope/dep"), None));
        }

        // Alias forms resolve by the aliased target name (scoped and unscoped).
        assert_eq!(
            target("@scope/dep@*", "alias-key"),
            (Some("@scope/dep"), None)
        );
        assert_eq!(target("plain@^1.0.0", "alias-key"), (Some("plain"), None));

        // Path forms resolve by relative path.
        assert_eq!(target("./pkg", "k"), (None, Some("./pkg")));
        assert_eq!(target("../pkg", "k"), (None, Some("../pkg")));
    }

    #[test]
    fn test_parse_workspace_protocol_resolves_all_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // pnpm workspace root — members live in pnpm-workspace.yaml, not package.json.
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();

        let star = write_member(&root, "packages/star", "@scope/star");
        let caret = write_member(&root, "packages/caret", "@scope/caret");
        let tilde = write_member(&root, "packages/tilde", "@scope/tilde");
        let pinned = write_member(&root, "packages/pinned", "@scope/pinned");
        let aliased = write_member(&root, "packages/aliased", "@scope/aliased");
        let pathdep = write_member(&root, "packages/pathdep", "@scope/pathdep");

        // Consumer depends on each member via a different workspace: form.
        let consumer_dir = root.join("packages/consumer");
        std::fs::create_dir_all(&consumer_dir).unwrap();
        let consumer_pkg = consumer_dir.join("package.json");
        std::fs::write(
            &consumer_pkg,
            r#"{
                "name": "@scope/consumer",
                "scripts": {"build": "tsc"},
                "dependencies": {
                    "@scope/star": "workspace:*",
                    "@scope/caret": "workspace:^",
                    "@scope/tilde": "workspace:~1.2.3",
                    "@scope/pinned": "workspace:1.0.0",
                    "alias-key": "workspace:@scope/aliased@*",
                    "@scope/pathdep": "workspace:../pathdep"
                }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&consumer_pkg).unwrap();

        // Canonicalize to normalize the `..` left in the path form.
        let resolved: HashSet<PathBuf> = deps
            .iter()
            .map(|d| d.path.canonicalize().unwrap())
            .collect();

        for (label, expected) in [
            ("workspace:*", &star),
            ("workspace:^", &caret),
            ("workspace:~1.2.3", &tilde),
            ("workspace:1.0.0", &pinned),
            ("alias", &aliased),
            ("path", &pathdep),
        ] {
            let expected = expected.canonicalize().unwrap();
            assert!(
                resolved.contains(&expected),
                "{label} form should resolve to {}; got {resolved:?}",
                expected.display()
            );
        }
        assert_eq!(deps.len(), 6, "expected exactly six workspace deps");
    }

    #[test]
    fn test_parse_workspace_protocol_in_dev_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        let dep = write_member(&root, "packages/dep", "@scope/dep");

        let consumer_dir = root.join("packages/consumer");
        std::fs::create_dir_all(&consumer_dir).unwrap();
        let consumer_pkg = consumer_dir.join("package.json");
        std::fs::write(
            &consumer_pkg,
            r#"{
                "name": "@scope/consumer",
                "scripts": {"build": "tsc"},
                "devDependencies": { "@scope/dep": "workspace:*" }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&consumer_pkg).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@scope/dep");
        assert_eq!(deps[0].path, dep);
    }

    #[test]
    fn test_workspace_protocol_end_to_end_targets_pnpm() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // pnpm workspace with a lockfile so the members resolve to pnpm commands.
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();

        let _dep = write_member(&root, "packages/dep", "@scope/dep");

        let consumer_dir = root.join("packages/consumer");
        std::fs::create_dir_all(&consumer_dir).unwrap();
        let consumer_pkg = consumer_dir.join("package.json");
        std::fs::write(
            &consumer_pkg,
            r#"{
                "name": "@scope/consumer",
                "scripts": {"build": "tsc"},
                "dependencies": { "@scope/dep": "workspace:*" }
            }"#,
        )
        .unwrap();

        let plugin = NodeJsPlugin;
        let deps = plugin.parse_dependencies(&consumer_pkg).unwrap();
        let ctx = TargetContext {
            config_path: &consumer_pkg,
            project_dir: &consumer_dir,
            workspace_root: &root,
            dependencies: &deps,
        };
        let targets = plugin.detect_targets(&ctx).unwrap();

        // The headline fix: consumer:build must depend on the dependency's build,
        // so it is ordered after it.
        let build = targets.get("build").unwrap();
        assert!(
            build
                .depends_on
                .contains(&"//packages/dep:build".to_string()),
            "consumer:build should depend on //packages/dep:build; got {:?}",
            build.depends_on
        );
    }

    #[test]
    fn packaged_ruby_project_owns_colocated_javascript_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let package = tmp.path().join("package.json");
        std::fs::write(&package, r#"{"name":"assets"}"#).unwrap();
        std::fs::write(tmp.path().join("app.gemspec"), "s.name = 'app'\n").unwrap();

        assert!(NodeJsPlugin.should_skip(&package));
    }
}
