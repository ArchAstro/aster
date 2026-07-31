//! Gradle language plugin for JVM projects.

use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{CacheInputs, LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetContext};

static ROOT_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)\brootProject\.name\s*=\s*["']([^"']+)["']"#)
        .expect("valid Gradle root name regex")
});
static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*version\s*=\s*["']([^"']+)["']"#).expect("valid Gradle version regex")
});
static PROJECT_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"project\s*\(\s*["'](:[^"']+)["']\s*\)\.projectDir\s*=\s*file\s*\(\s*["']([^"']+)["']\s*\)"#,
    )
    .expect("valid Gradle project directory regex")
});
static QUOTED_PROJECT_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"["'](:[A-Za-z0-9_.:-]+)["']"#).expect("valid Gradle project path regex")
});
static INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)\binclude\s*(?:\(|\s)"#).expect("valid include regex"));

/// Plugin for Gradle Groovy and Kotlin DSL builds.
pub struct GradlePlugin;

impl LanguagePlugin for GradlePlugin {
    fn name(&self) -> &str {
        "gradle"
    }

    fn marker_files(&self) -> &[&str] {
        &[
            "settings.gradle",
            "settings.gradle.kts",
            "build.gradle",
            "build.gradle.kts",
        ]
    }

    fn matches_marker(&self, filename: &str) -> bool {
        filename.ends_with(".gradle") || filename.ends_with(".gradle.kts")
    }

    fn should_skip(&self, config_path: &Path) -> bool {
        let Some(project_dir) = config_path.parent() else {
            return true;
        };
        let Some(filename) = config_path.file_name().and_then(|name| name.to_str()) else {
            return true;
        };

        // A directory with both build systems needs one unambiguous Aster
        // address. Prefer the declarative Maven model; users can still override
        // its targets in aster.toml.
        if project_dir.join("pom.xml").is_file() || project_dir.join("package.json").is_file() {
            return true;
        }

        if is_settings_file(filename) {
            // Prefer Kotlin settings when both files are present.
            if filename == "settings.gradle" && project_dir.join("settings.gradle.kts").is_file() {
                return true;
            }

            // Pure aggregator roots are orchestration containers, not source
            // projects. Their subprojects are discovered independently.
            let content = std::fs::read_to_string(config_path).unwrap_or_default();
            return !project_dir.join("src").is_dir() && INCLUDE.is_match(&content);
        }

        // A settings file is the canonical marker for a source-bearing root.
        if has_settings_file(project_dir) {
            return true;
        }

        if filename == "build.gradle" && project_dir.join("build.gradle.kts").is_file() {
            return true;
        }
        if filename == "build.gradle" || filename == "build.gradle.kts" {
            return false;
        }

        // Gradle permits arbitrary build filenames. Accept the widespread
        // `<project-directory>.gradle(.kts)` convention while rejecting
        // convention plugins and auxiliary scripts elsewhere in the tree.
        let dir_name = project_dir.file_name().and_then(|name| name.to_str());
        dir_name.is_none_or(|dir_name| {
            filename != format!("{dir_name}.gradle") && filename != format!("{dir_name}.gradle.kts")
        })
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let filename = config_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        let name = if is_settings_file(filename) {
            ROOT_NAME
                .captures(&content)
                .and_then(|captures| captures.get(1))
                .map(|name| name.as_str().to_string())
                .or_else(|| directory_name(config_path.parent()?))
                .unwrap_or_else(|| "gradle-project".to_string())
        } else {
            gradle_build_name(filename)
                .or_else(|| directory_name(config_path.parent()?))
                .unwrap_or_else(|| "gradle-project".to_string())
        };
        let version = VERSION
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .map(|version| version.as_str().to_string());

        Ok(ProjectMetadata { name, version })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        // Gradle relationships are configuration- and task-specific. Scoped
        // Gradle tasks preserve the native graph; unconditional Aster project
        // edges can turn valid test/reporting relationships into false cycles.
        let _ = config_path;
        Ok(Vec::new())
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let build_root = find_gradle_root(ctx.project_dir);
        let task_prefix = gradle_task_prefix(&build_root, ctx.project_dir);
        let runner = gradle_runner(&build_root);
        let resource = native_build_resource("gradle", ctx.workspace_root, &build_root);
        let working_dir = Some(build_root.clone());
        // Gradle's task graph owns relationships between projects in the same
        // build. Flattening configuration-specific project dependencies into
        // unconditional Aster :build edges can invent cycles.
        let base_dependencies = vec!["//self:deps".to_string()];

        let task = |name: &str| {
            if task_prefix.is_empty() {
                name.to_string()
            } else {
                format!("{task_prefix}:{name}")
            }
        };
        let make_target =
            |command: String, depends_on: Vec<String>, invalidates_cache: bool| Target {
                command,
                depends_on,
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache,
                working_dir: working_dir.clone(),
                exclusive_resources: vec![resource.clone()],
            };

        let mut targets = HashMap::new();
        targets.insert(
            "deps".to_string(),
            make_target(format!("{runner} {}", task("dependencies")), vec![], false),
        );
        targets.insert(
            "build".to_string(),
            make_target(
                format!("{runner} {}", task("build")),
                base_dependencies.clone(),
                false,
            ),
        );
        targets.insert(
            "test".to_string(),
            make_target(
                format!("{runner} {}", task("test")),
                base_dependencies.clone(),
                false,
            ),
        );
        targets.insert(
            "lint".to_string(),
            make_target(
                format!("{runner} {}", task("check")),
                base_dependencies,
                false,
            ),
        );

        let build_content = std::fs::read_to_string(ctx.config_path).unwrap_or_default();
        let root_content = std::fs::read_to_string(build_root.join("build.gradle.kts"))
            .or_else(|_| std::fs::read_to_string(build_root.join("build.gradle")))
            .unwrap_or_default();
        let all_content = format!("{root_content}\n{build_content}");
        let format_task = if all_content.contains("spotless") {
            Some("spotlessApply")
        } else if all_content.contains("ktlint") {
            Some("ktlintFormat")
        } else {
            None
        };
        if let Some(format_task) = format_task {
            targets.insert(
                "format".to_string(),
                make_target(
                    format!("{runner} {}", task(format_task)),
                    vec!["//self:deps".to_string()],
                    false,
                ),
            );
        }
        targets.insert(
            "clean".to_string(),
            make_target(format!("{runner} {}", task("clean")), vec![], true),
        );

        Ok(targets)
    }

    fn cache_inputs(&self, target_name: &str) -> CacheInputs {
        let mut inputs = CacheInputs {
            source_globs: Vec::new(),
            config_files: vec![
                "settings.gradle".to_string(),
                "settings.gradle.kts".to_string(),
                "build.gradle".to_string(),
                "build.gradle.kts".to_string(),
                "gradle.properties".to_string(),
                "gradle/libs.versions.toml".to_string(),
                "gradle/wrapper/gradle-wrapper.properties".to_string(),
            ],
            env_vars: vec![
                "JAVA_HOME".to_string(),
                "GRADLE_OPTS".to_string(),
                "ORG_GRADLE_PROJECT_*".to_string(),
                "CI".to_string(),
            ],
        };
        if target_name != "deps" {
            inputs.source_globs = vec![
                "src/**/*.java".to_string(),
                "src/**/*.kt".to_string(),
                "src/**/*.kts".to_string(),
                "src/**/*.groovy".to_string(),
                "src/**/*.scala".to_string(),
                "src/main/resources/**/*".to_string(),
            ];
        }
        if target_name == "test" || target_name == "lint" {
            inputs.source_globs.extend([
                "src/test/**/*".to_string(),
                "src/integrationTest/**/*".to_string(),
                "src/testFixtures/**/*".to_string(),
            ]);
        }
        inputs
    }
}

fn is_settings_file(filename: &str) -> bool {
    filename == "settings.gradle" || filename == "settings.gradle.kts"
}

fn has_settings_file(directory: &Path) -> bool {
    directory.join("settings.gradle").is_file() || directory.join("settings.gradle.kts").is_file()
}

fn directory_name(directory: &Path) -> Option<String> {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn gradle_build_name(filename: &str) -> Option<String> {
    filename
        .strip_suffix(".gradle.kts")
        .or_else(|| filename.strip_suffix(".gradle"))
        .filter(|name| *name != "build")
        .map(str::to_string)
}

fn find_gradle_root(project_dir: &Path) -> PathBuf {
    project_dir
        .ancestors()
        .find(|directory| has_settings_file(directory))
        .unwrap_or(project_dir)
        .to_path_buf()
}

fn read_settings(build_root: &Path) -> String {
    std::fs::read_to_string(build_root.join("settings.gradle.kts"))
        .or_else(|_| std::fs::read_to_string(build_root.join("settings.gradle")))
        .unwrap_or_default()
}

fn project_directory_mappings(build_root: &Path, settings: &str) -> HashMap<String, PathBuf> {
    PROJECT_DIR
        .captures_iter(settings)
        .map(|captures| {
            (
                captures
                    .get(1)
                    .expect("capture exists")
                    .as_str()
                    .to_string(),
                build_root.join(captures.get(2).expect("capture exists").as_str()),
            )
        })
        .collect()
}

fn gradle_task_prefix(build_root: &Path, project_dir: &Path) -> String {
    if build_root == project_dir {
        return String::new();
    }
    let settings = read_settings(build_root);
    let mappings = project_directory_mappings(build_root, &settings);
    let canonical_project = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    if let Some((project_path, _)) = mappings.into_iter().find(|(_, directory)| {
        directory
            .canonicalize()
            .unwrap_or_else(|_| directory.to_path_buf())
            == canonical_project
    }) {
        return project_path;
    }
    let relative = project_dir.strip_prefix(build_root).unwrap_or(project_dir);
    let default_path = format!(
        ":{}",
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join(":")
    );
    let included_paths: Vec<&str> = QUOTED_PROJECT_PATH
        .captures_iter(&settings)
        .filter_map(|captures| captures.get(1).map(|path| path.as_str()))
        .collect();
    if included_paths.contains(&default_path.as_str()) {
        return default_path;
    }

    // Some builds derive projectDir programmatically from a longer logical
    // name (for example `:vendor-examples-api` -> `api`). Use that mapping only
    // when the suffix match is unique; ambiguity falls back to the filesystem
    // path and lets Gradle fail loudly.
    let normalized_relative = normalize_gradle_path(&default_path);
    let suffix_matches: Vec<&str> = included_paths
        .into_iter()
        .filter(|path| normalize_gradle_path(path).ends_with(&normalized_relative))
        .collect();
    if suffix_matches.len() == 1 {
        return suffix_matches[0].to_string();
    }
    default_path
}

fn normalize_gradle_path(path: &str) -> String {
    path.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn gradle_runner(build_root: &Path) -> String {
    if cfg!(windows) && build_root.join("gradlew.bat").is_file() {
        "gradlew.bat".to_string()
    } else if build_root.join("gradlew").is_file() {
        "./gradlew".to_string()
    } else {
        "gradle".to_string()
    }
}

fn native_build_resource(kind: &str, workspace_root: &Path, build_root: &Path) -> String {
    let root = build_root
        .strip_prefix(workspace_root)
        .unwrap_or(build_root)
        .to_string_lossy();
    format!("{kind}-build:{root}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(
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
    fn parses_kotlin_dsl_root_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.gradle.kts");
        std::fs::write(
            &settings,
            r#"
rootProject.name = "payments"
include("api")
"#,
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("src/main/java")).unwrap();

        let metadata = GradlePlugin.parse_project(temp.path(), &settings).unwrap();
        assert_eq!(metadata.name, "payments");
        assert_eq!(metadata.version, None);
        assert!(!GradlePlugin.should_skip(&settings));
    }

    #[test]
    fn skips_pure_aggregator_and_duplicate_root_build_file() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.gradle.kts");
        let build = temp.path().join("build.gradle.kts");
        std::fs::write(&settings, "include(\"api\")").unwrap();
        std::fs::write(&build, "plugins { java }").unwrap();

        assert!(GradlePlugin.should_skip(&settings));
        assert!(GradlePlugin.should_skip(&build));
    }

    #[test]
    fn recognizes_safe_custom_build_filename_but_not_auxiliary_script() {
        let temp = tempfile::tempdir().unwrap();
        let module = temp.path().join("api");
        std::fs::create_dir_all(&module).unwrap();
        let custom = module.join("api.gradle.kts");
        let auxiliary = module.join("publishing.gradle.kts");
        std::fs::write(&custom, "plugins { java }").unwrap();
        std::fs::write(&auxiliary, "").unwrap();

        assert!(GradlePlugin.matches_marker("api.gradle.kts"));
        assert!(!GradlePlugin.should_skip(&custom));
        assert!(GradlePlugin.should_skip(&auxiliary));
    }

    #[test]
    fn defers_project_dependencies_to_gradle() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("settings.gradle.kts"),
            r#"include("shared", "services:api")"#,
        )
        .unwrap();
        for path in ["shared", "services/api"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
            std::fs::write(root.join(path).join("build.gradle.kts"), "").unwrap();
        }
        let api_build = root.join("services/api/build.gradle.kts");
        std::fs::write(
            &api_build,
            r#"
dependencies {
    implementation(project(":shared"))
    testImplementation(projects.shared)
}
"#,
        )
        .unwrap();

        assert!(GradlePlugin
            .parse_dependencies(&api_build)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn scopes_wrapper_targets_to_module_and_defers_native_dependency_ordering() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("settings.gradle"), "include 'lib', 'app'").unwrap();
        std::fs::write(root.join("gradlew"), "").unwrap();
        for module in ["lib", "app"] {
            std::fs::create_dir_all(root.join(module)).unwrap();
            std::fs::write(
                root.join(module).join("build.gradle"),
                "apply plugin: 'java'",
            )
            .unwrap();
        }
        let app_build = root.join("app/build.gradle");
        let dependencies = vec![LocalDependency {
            name: "lib".to_string(),
            path: root.join("lib"),
        }];
        let ctx = context(&app_build, &root, &dependencies);
        let targets = GradlePlugin.detect_targets(&ctx).unwrap();

        let build = &targets["build"];
        assert_eq!(build.command, "./gradlew :app:build");
        assert_eq!(build.working_dir.as_deref(), Some(root.as_path()));
        assert_eq!(build.depends_on, vec!["//self:deps"]);
        assert_eq!(build.exclusive_resources, vec!["gradle-build:"]);
        assert_eq!(targets["clean"].command, "./gradlew :app:clean");
        assert!(targets["clean"].invalidates_cache);
    }

    #[test]
    fn resolves_logical_project_name_that_differs_from_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("settings.gradle.kts"),
            r#"include(":vendor-examples-api", ":vendor-examples-worker")"#,
        )
        .unwrap();
        std::fs::write(root.join("gradlew"), "").unwrap();
        let module = root.join("api");
        std::fs::create_dir_all(&module).unwrap();
        let build = module.join("build.gradle.kts");
        std::fs::write(&build, "plugins { java }").unwrap();

        let targets = GradlePlugin
            .detect_targets(&context(&build, &root, &[]))
            .unwrap();
        assert_eq!(
            targets["test"].command,
            "./gradlew :vendor-examples-api:test"
        );
    }

    #[test]
    fn resolves_explicit_custom_project_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("settings.gradle"),
            "include ':api'\nproject(':api').projectDir = file('services/backend')\n",
        )
        .unwrap();
        let module = root.join("services/backend");
        std::fs::create_dir_all(&module).unwrap();
        let build = module.join("build.gradle");
        std::fs::write(&build, "apply plugin: 'java'").unwrap();

        let targets = GradlePlugin
            .detect_targets(&context(&build, &root, &[]))
            .unwrap();
        assert_eq!(targets["build"].command, "gradle :api:build");
    }

    #[test]
    fn detects_format_tool_and_cache_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let build = temp.path().join("build.gradle.kts");
        std::fs::write(&build, r#"plugins { id("com.diffplug.spotless") }"#).unwrap();
        let ctx = context(&build, temp.path(), &[]);
        let targets = GradlePlugin.detect_targets(&ctx).unwrap();

        assert_eq!(targets["format"].command, "gradle spotlessApply");
        assert!(GradlePlugin.cache_inputs("deps").source_globs.is_empty());
        assert!(GradlePlugin
            .cache_inputs("test")
            .source_globs
            .contains(&"src/test/**/*".to_string()));
    }

    #[test]
    fn existing_non_gradle_marker_wins_colocated_address() {
        let temp = tempfile::tempdir().unwrap();
        let build = temp.path().join("build.gradle");
        std::fs::write(&build, "").unwrap();
        std::fs::write(temp.path().join("pom.xml"), "<project/>").unwrap();
        assert!(GradlePlugin.should_skip(&build));
    }
}
