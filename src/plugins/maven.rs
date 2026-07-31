//! Maven language plugin for JVM projects.

use anyhow::{anyhow, Context, Result};
use roxmltree::{Document, Node};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::jvm;
use super::{
    CacheInputs, LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetCapability,
    TargetContext,
};

/// Plugin for Maven JVM builds.
pub struct MavenPlugin;

impl LanguagePlugin for MavenPlugin {
    fn name(&self) -> &str {
        "maven"
    }

    fn languages(&self, project_dir: &Path, config_path: &Path) -> Result<Vec<String>> {
        let mut languages = jvm::source_languages(project_dir);
        let content = std::fs::read_to_string(config_path).unwrap_or_default();
        let has_kotlin_plugin = content.contains("kotlin-maven-plugin");

        if has_kotlin_plugin && !languages.iter().any(|language| language == "kotlin") {
            languages.push("kotlin".to_string());
        }
        // Java is Maven's default source language when no Kotlin evidence is
        // present. This also gives empty starter projects a useful language.
        if languages.is_empty() {
            languages.push(if has_kotlin_plugin { "kotlin" } else { "java" }.to_string());
        }
        languages.sort();
        Ok(languages)
    }

    fn build_system(&self) -> Option<&str> {
        Some("maven")
    }

    fn marker_files(&self) -> &[&str] {
        &["pom.xml"]
    }

    fn should_skip(&self, config_path: &Path) -> bool {
        let Some(project_dir) = config_path.parent() else {
            return true;
        };
        let components: Vec<_> = config_path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect();
        if components
            .windows(2)
            .any(|pair| pair == ["src", "it"] || pair == ["src", "test"])
        {
            return true;
        }
        let Ok(content) = std::fs::read_to_string(config_path) else {
            return false;
        };
        let Ok(document) = Document::parse(&content) else {
            return false;
        };
        let project = document.root_element();
        let has_modules = child(project, "modules")
            .is_some_and(|modules| children(modules, "module").next().is_some());
        has_modules && !has_jvm_sources(project_dir)
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let document = Document::parse(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;
        let project = document.root_element();
        let name = child_text(project, "artifactId")
            .ok_or_else(|| anyhow!("Missing <artifactId> in {}", config_path.display()))?
            .to_string();
        let version = child_text(project, "version")
            .or_else(|| child(project, "parent").and_then(|parent| child_text(parent, "version")))
            .map(str::to_string);

        Ok(ProjectMetadata { name, version })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        // Maven's reactor owns dependency scopes, plugin dependencies, build
        // extensions, and ordering. `-pl ... -am` preserves that exact model.
        let _ = config_path;
        Ok(Vec::new())
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let reactor_root = find_maven_reactor_root(ctx.project_dir);
        let runner = maven_runner(&reactor_root);
        let artifact = read_maven_artifact(ctx.config_path);
        let is_module = reactor_root != ctx.project_dir;
        let module_args = if is_module {
            artifact
                .as_deref()
                .map(|artifact| format!("-pl :{artifact}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let resource = native_build_resource("maven", ctx.workspace_root, reactor_root.as_path());
        let working_dir = Some(reactor_root.clone());
        // Maven's reactor owns module ordering. `-am` selects the required
        // reactor dependencies with their actual Maven scopes; unconditional
        // Aster build edges would duplicate work and can invent cycles.
        let base_dependencies = vec!["//self:deps".to_string()];

        let command = |goals: &str, also_make: bool| {
            let mut parts = vec![runner.clone()];
            if !module_args.is_empty() {
                parts.push(module_args.clone());
                if also_make {
                    parts.push("-am".to_string());
                }
            }
            parts.push(goals.to_string());
            parts.join(" ")
        };
        let make_target = |command: String,
                           depends_on: Vec<String>,
                           invalidates_cache: bool,
                           warnings_as_errors: bool| {
            let mut capabilities = HashSet::new();
            if warnings_as_errors {
                capabilities.insert(TargetCapability::WarningsAsErrors);
            }
            Target {
                command,
                depends_on,
                capabilities,
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache,
                working_dir: working_dir.clone(),
                exclusive_resources: vec![resource.clone()],
            }
        };

        let mut targets = HashMap::new();
        targets.insert(
            "deps".to_string(),
            make_target(command("dependency:go-offline", true), vec![], false, false),
        );
        targets.insert(
            "build".to_string(),
            make_target(
                command("package -DskipTests", true),
                base_dependencies.clone(),
                false,
                true,
            ),
        );
        targets.insert(
            "test".to_string(),
            make_target(
                command("test", true),
                base_dependencies.clone(),
                false,
                true,
            ),
        );
        targets.insert(
            "lint".to_string(),
            make_target(
                command("verify -DskipTests", true),
                base_dependencies,
                false,
                true,
            ),
        );

        let content = std::fs::read_to_string(ctx.config_path).unwrap_or_default();
        let format_goal = if content.contains("spotless-maven-plugin") {
            Some("spotless:apply")
        } else if content.contains("fmt-maven-plugin") {
            Some("fmt:format")
        } else {
            None
        };
        if let Some(format_goal) = format_goal {
            targets.insert(
                "format".to_string(),
                make_target(
                    command(format_goal, true),
                    vec!["//self:deps".to_string()],
                    false,
                    false,
                ),
            );
        }
        targets.insert(
            "clean".to_string(),
            make_target(command("clean", false), vec![], true, false),
        );
        Ok(targets)
    }

    fn with_warnings_as_errors(&self, target_name: &str, command: &str) -> Option<String> {
        matches!(target_name, "build" | "test" | "lint")
            .then(|| format!("{command} -Dmaven.compiler.failOnWarning=true"))
    }

    fn cache_inputs(&self, target_name: &str) -> CacheInputs {
        let mut inputs = CacheInputs {
            source_globs: Vec::new(),
            config_files: vec![
                "pom.xml".to_string(),
                ".mvn/**/*".to_string(),
                "mvnw".to_string(),
                "mvnw.cmd".to_string(),
            ],
            env_vars: vec![
                "JAVA_HOME".to_string(),
                "MAVEN_OPTS".to_string(),
                "MAVEN_ARGS".to_string(),
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
            inputs
                .source_globs
                .extend(["src/test/**/*".to_string(), "src/it/**/*".to_string()]);
        }
        inputs
    }
}

fn child<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

fn children<'a>(node: Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = Node<'a, 'a>> + 'a {
    node.children()
        .filter(move |child| child.is_element() && child.tag_name().name() == name)
}

fn child_text<'a>(node: Node<'a, 'a>, name: &str) -> Option<&'a str> {
    child(node, name)?
        .text()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn has_jvm_sources(project_dir: &Path) -> bool {
    ["java", "kotlin", "groovy", "scala"]
        .iter()
        .any(|language| project_dir.join("src/main").join(language).is_dir())
}

fn read_maven_artifact(config_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(config_path).ok()?;
    let document = Document::parse(&content).ok()?;
    child_text(document.root_element(), "artifactId").map(str::to_string)
}

fn pom_modules(pom_path: &Path) -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(pom_path) else {
        return Vec::new();
    };
    let Ok(document) = Document::parse(&content) else {
        return Vec::new();
    };
    child(document.root_element(), "modules")
        .map(|modules| {
            children(modules, "module")
                .filter_map(|module| module.text())
                .map(str::trim)
                .filter(|module| !module.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn find_maven_reactor_root(project_dir: &Path) -> PathBuf {
    let canonical_project = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let mut reactor_root = project_dir.to_path_buf();

    for ancestor in project_dir.ancestors().skip(1) {
        let pom = ancestor.join("pom.xml");
        if !pom.is_file() {
            continue;
        }
        let contains_project = pom_modules(&pom).into_iter().any(|module| {
            let module_dir = ancestor.join(module);
            let canonical_module = module_dir.canonicalize().unwrap_or(module_dir);
            canonical_project.starts_with(canonical_module)
        });
        if contains_project {
            reactor_root = ancestor.to_path_buf();
        }
    }
    reactor_root
}

fn maven_runner(reactor_root: &Path) -> String {
    if cfg!(windows) && reactor_root.join("mvnw.cmd").is_file() {
        "mvnw.cmd".to_string()
    } else if reactor_root.join("mvnw").is_file() {
        "./mvnw".to_string()
    } else {
        "mvn".to_string()
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
    fn parses_namespaced_pom_and_inherited_version() {
        let temp = tempfile::tempdir().unwrap();
        let pom = temp.path().join("pom.xml");
        std::fs::write(
            &pom,
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <parent>
    <groupId>dev.example</groupId>
    <artifactId>parent</artifactId>
    <version>2.1.0</version>
  </parent>
  <artifactId>api</artifactId>
</project>"#,
        )
        .unwrap();

        let metadata = MavenPlugin.parse_project(temp.path(), &pom).unwrap();
        assert_eq!(metadata.name, "api");
        assert_eq!(metadata.version.as_deref(), Some("2.1.0"));
    }

    #[test]
    fn defers_reactor_dependencies_to_maven() {
        let temp = tempfile::tempdir().unwrap();
        let pom = temp.path().join("pom.xml");
        std::fs::write(
            &pom,
            r#"<project>
  <artifactId>app</artifactId>
  <dependencyManagement><dependencies><dependency>
    <artifactId>managed</artifactId>
  </dependency></dependencies></dependencyManagement>
  <dependencies>
    <dependency><groupId>dev.example</groupId><artifactId>shared</artifactId></dependency>
  </dependencies>
</project>"#,
        )
        .unwrap();

        assert!(MavenPlugin.parse_dependencies(&pom).unwrap().is_empty());
    }

    #[test]
    fn skips_pure_aggregator_but_keeps_source_bearing_parent() {
        let temp = tempfile::tempdir().unwrap();
        let pom = temp.path().join("pom.xml");
        std::fs::write(
            &pom,
            "<project><artifactId>root</artifactId><packaging>pom</packaging><modules><module>api</module></modules></project>",
        )
        .unwrap();
        assert!(MavenPlugin.should_skip(&pom));

        std::fs::create_dir_all(temp.path().join("src/main/java")).unwrap();
        assert!(!MavenPlugin.should_skip(&pom));
    }

    #[test]
    fn skips_embedded_maven_test_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("src/it/sample/pom.xml");
        std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        std::fs::write(
            &fixture,
            "<project><artifactId>fixture</artifactId></project>",
        )
        .unwrap();
        assert!(MavenPlugin.should_skip(&fixture));
    }

    #[test]
    fn scopes_wrapper_targets_to_reactor_module() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("pom.xml"),
            "<project><artifactId>root</artifactId><modules><module>api</module></modules></project>",
        )
        .unwrap();
        std::fs::write(root.join("mvnw"), "").unwrap();
        let module = root.join("api");
        std::fs::create_dir_all(module.join("src/main/java")).unwrap();
        let pom = module.join("pom.xml");
        std::fs::write(&pom, "<project><artifactId>api</artifactId></project>").unwrap();
        let dependencies = vec![LocalDependency {
            name: "shared".to_string(),
            path: PathBuf::from("//shared"),
        }];
        let ctx = context(&pom, &root, &dependencies);
        let targets = MavenPlugin.detect_targets(&ctx).unwrap();

        assert_eq!(
            targets["build"].command,
            "./mvnw -pl :api -am package -DskipTests"
        );
        assert_eq!(targets["test"].command, "./mvnw -pl :api -am test");
        assert_eq!(targets["clean"].command, "./mvnw -pl :api clean");
        assert_eq!(
            targets["build"].working_dir.as_deref(),
            Some(root.as_path())
        );
        assert_eq!(targets["build"].depends_on, vec!["//self:deps"]);
        assert_eq!(targets["build"].exclusive_resources, vec!["maven-build:"]);
    }

    #[test]
    fn standalone_project_uses_system_maven() {
        let temp = tempfile::tempdir().unwrap();
        let pom = temp.path().join("pom.xml");
        std::fs::write(&pom, "<project><artifactId>api</artifactId></project>").unwrap();
        let targets = MavenPlugin
            .detect_targets(&context(&pom, temp.path(), &[]))
            .unwrap();
        assert_eq!(targets["build"].command, "mvn package -DskipTests");
    }

    #[test]
    fn detects_spotless_and_warnings_as_errors() {
        let temp = tempfile::tempdir().unwrap();
        let pom = temp.path().join("pom.xml");
        std::fs::write(
            &pom,
            "<project><artifactId>api</artifactId><build><plugins><plugin><artifactId>spotless-maven-plugin</artifactId></plugin></plugins></build></project>",
        )
        .unwrap();
        let targets = MavenPlugin
            .detect_targets(&context(&pom, temp.path(), &[]))
            .unwrap();
        assert_eq!(targets["format"].command, "mvn spotless:apply");
        assert_eq!(
            MavenPlugin.with_warnings_as_errors("build", &targets["build"].command),
            Some("mvn package -DskipTests -Dmaven.compiler.failOnWarning=true".to_string())
        );
        assert_eq!(
            MavenPlugin.with_warnings_as_errors("clean", "mvn clean"),
            None
        );
    }

    #[test]
    fn dependency_cache_excludes_sources() {
        assert!(MavenPlugin.cache_inputs("deps").source_globs.is_empty());
        assert!(MavenPlugin
            .cache_inputs("test")
            .source_globs
            .contains(&"src/test/**/*".to_string()));
    }
}
