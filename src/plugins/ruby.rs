//! Ruby language plugin for Bundler, RubyGems, Rake, RSpec, and Rails projects.

use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::{
    CacheInputs, LanguagePlugin, LocalDependency, ProjectMetadata, Target, TargetCapability,
    TargetContext,
};

static GEM_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*[A-Za-z_]\w*\.name\s*=\s*[\"']([^\"']+)[\"']"#)
        .expect("valid gem name regex")
});
static GEM_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*[A-Za-z_]\w*\.version\s*=\s*[\"']([^\"']+)[\"']"#)
        .expect("valid gem version regex")
});
static GEMSPEC_DEPENDENCY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\s*[A-Za-z_]\w*\.add_(?:runtime_)?dependency\s*(?:\(\s*)?[\"']([^\"']+)[\"']"#,
    )
    .expect("valid gemspec dependency regex")
});
static GEMFILE_PATH_DEPENDENCY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\s*gem\s*(?:\(\s*)?[\"']([^\"']+)[\"'][^\n#]*(?:\bpath:\s*|:path\s*=>\s*)[\"']([^\"']+)[\"']"#,
    )
    .expect("valid Gemfile path dependency regex")
});

/// Ruby plugin for Bundler projects and packaged gems.
pub struct RubyPlugin;

impl LanguagePlugin for RubyPlugin {
    fn name(&self) -> &str {
        "ruby"
    }

    fn marker_files(&self) -> &[&str] {
        &["Gemfile"]
    }

    fn matches_marker(&self, filename: &str) -> bool {
        filename == "Gemfile" || filename.ends_with(".gemspec")
    }

    fn should_skip(&self, config_path: &Path) -> bool {
        if is_fixture_gemspec(config_path) {
            return true;
        }

        let Some(project_dir) = config_path.parent() else {
            return true;
        };
        let is_gemfile = config_path
            .file_name()
            .is_some_and(|name| name == "Gemfile");
        let gemspecs = gemspecs_in(project_dir);

        // A gem's specification contains better metadata than its colocated
        // Gemfile. Use exactly one marker so discovery cannot produce two
        // projects with the same Aster address.
        if is_gemfile {
            return !gemspecs.is_empty();
        }

        gemspecs
            .first()
            .is_some_and(|canonical| canonical != config_path)
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let is_gemspec = config_path.extension().is_some_and(|ext| ext == "gemspec");

        let name = if is_gemspec {
            GEM_NAME
                .captures(&content)
                .and_then(|captures| captures.get(1))
                .map(|value| value.as_str().to_string())
                .or_else(|| {
                    config_path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                })
        } else {
            config_path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
        }
        .unwrap_or_else(|| "ruby-project".to_string());

        let version = GEM_VERSION
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().to_string());

        Ok(ProjectMetadata { name, version })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let mut dependencies = Vec::new();
        let config_content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        if config_path.extension().is_some_and(|ext| ext == "gemspec") {
            for captures in GEMSPEC_DEPENDENCY.captures_iter(&config_content) {
                let name = captures[1].to_string();
                dependencies.push(LocalDependency {
                    path: PathBuf::from(format!("ruby_workspace:{name}")),
                    name,
                });
            }
        }

        let project_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        let gemfile = project_dir.join("Gemfile");
        let gemfile_content = if config_path
            .file_name()
            .is_some_and(|name| name == "Gemfile")
        {
            Some(config_content.clone())
        } else {
            std::fs::read_to_string(&gemfile).ok()
        };

        if let Some(content) = gemfile_content.as_deref() {
            for captures in GEMFILE_PATH_DEPENDENCY.captures_iter(content) {
                dependencies.push(LocalDependency {
                    name: captures[1].to_string(),
                    path: PathBuf::from(&captures[2]),
                });
            }
        }

        dependencies
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
        dependencies.dedup_by(|left, right| left.name == right.name && left.path == right.path);
        Ok(dependencies)
    }

    fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
        let project_content = project_content(ctx.project_dir, ctx.config_path)?;
        let rakefile =
            std::fs::read_to_string(ctx.project_dir.join("Rakefile")).unwrap_or_default();
        let bundle_root = nearest_gemfile(ctx.project_dir, ctx.workspace_root)
            .and_then(|gemfile| gemfile.parent().map(Path::to_path_buf));
        let uses_bundler = bundle_root.is_some();
        let uses_ancestor_gemfile = bundle_root
            .as_deref()
            .is_some_and(|root| root != ctx.project_dir);
        let mut targets = HashMap::new();

        if uses_bundler {
            targets.insert(
                "deps".to_string(),
                target(
                    "bundle install",
                    Vec::new(),
                    HashSet::new(),
                    bundle_root,
                    vec!["bundler_install".to_string()],
                ),
            );
        }

        let base_dependencies = if uses_bundler {
            vec!["//self:deps".to_string()]
        } else {
            Vec::new()
        };
        let exec = if uses_bundler { "bundle exec " } else { "" };
        let is_gemspec = ctx
            .config_path
            .extension()
            .is_some_and(|extension| extension == "gemspec");
        if is_gemspec {
            let mut capabilities = HashSet::new();
            capabilities.insert(TargetCapability::WarningsAsErrors);
            let filename = ctx
                .config_path
                .file_name()
                .map(|name| crate::executor::quote_command_argument(&name.to_string_lossy()))
                .unwrap_or_else(|| "*.gemspec".to_string());
            targets.insert(
                "build".to_string(),
                target(
                    &format!("{exec}gem build {filename}"),
                    base_dependencies.clone(),
                    capabilities,
                    None,
                    Vec::new(),
                ),
            );
        }

        let test_dir = ctx.project_dir.join("test").is_dir();
        let spec_dir = ctx.project_dir.join("spec").is_dir();
        let rails_executable = ctx.project_dir.join("bin/rails").is_file();
        let (test_command, test_accepts_files) = if rails_executable && test_dir {
            (Some("bin/rails test".to_string()), true)
        } else if spec_dir
            && (ctx.project_dir.join(".rspec").exists()
                || project_content.contains("rspec")
                || rakefile.contains("RSpec::Core::RakeTask"))
        {
            (Some(format!("{exec}rspec")), true)
        } else if test_dir && rake_has_task(&rakefile, "test") {
            (Some(format!("{exec}rake test")), false)
        } else {
            (None, false)
        };

        if let Some(command) = test_command {
            let mut capabilities = HashSet::new();
            if test_accepts_files {
                capabilities.insert(TargetCapability::FilesList);
            }
            targets.insert(
                "test".to_string(),
                target(
                    &command,
                    base_dependencies.clone(),
                    capabilities,
                    None,
                    Vec::new(),
                ),
            );
        }

        let has_rubocop =
            ctx.project_dir.join(".rubocop.yml").exists() || project_content.contains("rubocop");
        if has_rubocop {
            let capabilities = HashSet::from([TargetCapability::FilesList]);
            targets.insert(
                "lint".to_string(),
                target(
                    &format!("{exec}rubocop"),
                    base_dependencies.clone(),
                    capabilities.clone(),
                    None,
                    Vec::new(),
                ),
            );
            targets.insert(
                "format".to_string(),
                target(
                    &format!("{exec}rubocop -A"),
                    base_dependencies.clone(),
                    capabilities,
                    None,
                    Vec::new(),
                ),
            );
        }

        if rails_executable {
            targets.insert(
                "dev".to_string(),
                Target {
                    stream: true,
                    ..target(
                        "bin/rails server",
                        base_dependencies,
                        HashSet::new(),
                        None,
                        Vec::new(),
                    )
                },
            );
        }

        if let Some(clean) = self.clean_target(ctx) {
            targets.insert("clean".to_string(), clean);
        }

        // Cache inputs are owned relative to the discovered project. A member
        // gem cannot safely hash a Gemfile.lock above its root, so favor
        // correctness and rerun its targets when Bundler is owned by an
        // ancestor project.
        if uses_ancestor_gemfile {
            for target in targets.values_mut() {
                target.cache = Some(crate::config::CacheConfig {
                    enabled: Some(false),
                    ..crate::config::CacheConfig::default()
                });
            }
        }

        Ok(targets)
    }

    fn with_files_list(
        &self,
        target_name: &str,
        command: &str,
        files: &[PathBuf],
    ) -> Option<String> {
        let matching = files
            .iter()
            .filter(|path| match target_name {
                "test" => is_ruby_test(path),
                "lint" | "format" => is_ruby_source(path),
                _ => false,
            })
            .map(|path| crate::executor::quote_command_argument(&path.to_string_lossy()))
            .collect::<Vec<_>>();

        if matching.is_empty() {
            None
        } else {
            Some(format!("{command} {}", matching.join(" ")))
        }
    }

    fn with_warnings_as_errors(&self, target_name: &str, command: &str) -> Option<String> {
        (target_name == "build").then(|| format!("{command} --strict"))
    }

    fn cache_inputs(&self, target_name: &str) -> CacheInputs {
        if target_name == "deps" {
            return CacheInputs {
                source_globs: Vec::new(),
                config_files: vec![
                    "Gemfile".to_string(),
                    "Gemfile.lock".to_string(),
                    "*.gemspec".to_string(),
                    ".ruby-version".to_string(),
                    ".tool-versions".to_string(),
                    ".bundle/config".to_string(),
                ],
                env_vars: vec![
                    "BUNDLE_GEMFILE".to_string(),
                    "BUNDLE_WITH".to_string(),
                    "BUNDLE_WITHOUT".to_string(),
                ],
            };
        }

        let mut source_globs = vec![
            "**/*.rb".to_string(),
            "**/*.rake".to_string(),
            "Rakefile".to_string(),
            "Gemfile".to_string(),
            "*.gemspec".to_string(),
        ];
        if target_name == "test" {
            source_globs.push("test/**/*".to_string());
            source_globs.push("spec/**/*".to_string());
        }
        CacheInputs {
            source_globs,
            config_files: vec![
                "Gemfile.lock".to_string(),
                ".ruby-version".to_string(),
                ".tool-versions".to_string(),
                ".rubocop.yml".to_string(),
            ],
            env_vars: vec![
                "RUBYOPT".to_string(),
                "RUBYLIB".to_string(),
                "BUNDLE_GEMFILE".to_string(),
                "RAILS_ENV".to_string(),
                "RACK_ENV".to_string(),
                "CI".to_string(),
            ],
        }
    }

    fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
        let rakefile = std::fs::read_to_string(ctx.project_dir.join("Rakefile")).ok()?;
        let task_name = if rake_has_task(&rakefile, "clobber") {
            "clobber"
        } else if rake_has_task(&rakefile, "clean") {
            "clean"
        } else {
            return None;
        };
        Some(Target {
            invalidates_cache: true,
            ..target(
                &format!(
                    "{}rake {task_name}",
                    if nearest_gemfile(ctx.project_dir, ctx.workspace_root).is_some() {
                        "bundle exec "
                    } else {
                        ""
                    }
                ),
                Vec::new(),
                HashSet::new(),
                None,
                Vec::new(),
            )
        })
    }
}

fn target(
    command: &str,
    depends_on: Vec<String>,
    capabilities: HashSet<TargetCapability>,
    working_dir: Option<PathBuf>,
    exclusive_resources: Vec<String>,
) -> Target {
    Target {
        command: command.to_string(),
        depends_on,
        capabilities,
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: false,
        working_dir,
        exclusive_resources,
    }
}

fn gemspecs_in(directory: &Path) -> Vec<PathBuf> {
    let mut gemspecs = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "gemspec")
        })
        .collect::<Vec<_>>();
    gemspecs.sort();
    gemspecs
}

fn is_fixture_gemspec(path: &Path) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    components.windows(2).any(|pair| {
        matches!(pair[0].as_ref(), "test" | "spec" | "features")
            && matches!(pair[1].as_ref(), "fixture" | "fixtures" | "support")
    })
}

fn nearest_gemfile(project_dir: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let mut directory = project_dir;
    loop {
        let candidate = directory.join("Gemfile");
        if candidate.is_file() {
            return Some(candidate);
        }
        if directory == workspace_root {
            return None;
        }
        directory = directory.parent()?;
    }
}

fn project_content(project_dir: &Path, config_path: &Path) -> Result<String> {
    let mut content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    for filename in ["Gemfile", "Gemfile.lock", "Rakefile"] {
        let path = project_dir.join(filename);
        if path != config_path {
            if let Ok(extra) = std::fs::read_to_string(path) {
                content.push('\n');
                content.push_str(&extra);
            }
        }
    }
    Ok(content)
}

fn rake_has_task(rakefile: &str, name: &str) -> bool {
    if name == "test" && rakefile.contains("Rake::TestTask.new") {
        return true;
    }
    let escaped = regex::escape(name);
    Regex::new(&format!(
        r#"(?m)^\s*task\s*(?:\(\s*)?(?::{}\b|[\"']{}[\"'])"#,
        escaped, escaped
    ))
    .expect("escaped task regex is valid")
    .is_match(rakefile)
}

fn is_ruby_test(path: &Path) -> bool {
    let value = path.to_string_lossy();
    let filename = path.file_name().map(|name| name.to_string_lossy());
    value.contains("/test/")
        || value.starts_with("test/")
        || value.contains("/spec/")
        || value.starts_with("spec/")
        || filename.as_deref().is_some_and(|name| {
            name.ends_with("_test.rb") || name.starts_with("test_") || name.ends_with("_spec.rb")
        })
}

fn is_ruby_source(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        matches!(
            extension.to_string_lossy().as_ref(),
            "rb" | "rake" | "gemspec"
        )
    }) || path
        .file_name()
        .is_some_and(|name| matches!(name.to_string_lossy().as_ref(), "Gemfile" | "Rakefile"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(config_path: &'a Path, root: &'a Path) -> TargetContext<'a> {
        TargetContext {
            config_path,
            project_dir: config_path.parent().unwrap(),
            workspace_root: root,
            dependencies: &[],
        }
    }

    #[test]
    fn gemspec_is_canonical_and_parses_literal_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let gemfile = tmp.path().join("Gemfile");
        let gemspec = tmp.path().join("sample.gemspec");
        std::fs::write(&gemfile, "gemspec\n").unwrap();
        std::fs::write(
            &gemspec,
            "Gem::Specification.new do |spec|\n  spec.name = 'sample'\n  spec.version = \"1.2.3\"\nend\n",
        )
        .unwrap();

        let plugin = RubyPlugin;
        assert!(plugin.should_skip(&gemfile));
        assert!(!plugin.should_skip(&gemspec));
        let metadata = plugin.parse_project(tmp.path(), &gemspec).unwrap();
        assert_eq!(metadata.name, "sample");
        assert_eq!(metadata.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn parses_literal_path_and_workspace_dependencies_without_evaluating_ruby() {
        let tmp = tempfile::tempdir().unwrap();
        let gemspec = tmp.path().join("app.gemspec");
        std::fs::write(
            &gemspec,
            "s.add_dependency 'shared', '~> 1.0'\ns.add_development_dependency 'rspec'\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("Gemfile"),
            "gem \"tools\", :path => \"../tools\"\ngem 'api', path: '../api'\n",
        )
        .unwrap();

        let dependencies = RubyPlugin.parse_dependencies(&gemspec).unwrap();
        assert_eq!(dependencies.len(), 3);
        assert!(dependencies.iter().any(|dependency| {
            dependency.name == "shared" && dependency.path == Path::new("ruby_workspace:shared")
        }));
        assert!(!dependencies
            .iter()
            .any(|dependency| dependency.name == "rspec"));
        assert!(dependencies
            .iter()
            .any(|dependency| dependency.path == Path::new("../tools")));
        assert!(dependencies
            .iter()
            .any(|dependency| dependency.path == Path::new("../api")));
    }

    #[test]
    fn detects_gem_rake_rspec_rubocop_and_clean_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let gemspec = tmp.path().join("sample.gemspec");
        std::fs::write(&gemspec, "s.name = 'sample'\ns.add_dependency 'rspec'\n").unwrap();
        std::fs::write(tmp.path().join("Gemfile"), "gem 'rubocop'\n").unwrap();
        std::fs::write(
            tmp.path().join("Rakefile"),
            "RSpec::Core::RakeTask.new(:spec)\ntask :clobber\n",
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join("spec")).unwrap();

        let targets = RubyPlugin
            .detect_targets(&context(&gemspec, tmp.path()))
            .unwrap();
        assert_eq!(targets["deps"].command, "bundle install");
        assert_eq!(
            targets["build"].command,
            "bundle exec gem build sample.gemspec"
        );
        assert_eq!(targets["test"].command, "bundle exec rspec");
        assert_eq!(targets["lint"].command, "bundle exec rubocop");
        assert_eq!(targets["format"].command, "bundle exec rubocop -A");
        assert_eq!(targets["clean"].command, "bundle exec rake clobber");
    }

    #[test]
    fn detects_rails_application_targets_and_file_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let gemfile = tmp.path().join("Gemfile");
        std::fs::write(&gemfile, "gem 'rails'\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("bin")).unwrap();
        std::fs::write(tmp.path().join("bin/rails"), "#!/usr/bin/env ruby\n").unwrap();
        std::fs::create_dir(tmp.path().join("test")).unwrap();

        let targets = RubyPlugin
            .detect_targets(&context(&gemfile, tmp.path()))
            .unwrap();
        assert_eq!(targets["test"].command, "bin/rails test");
        assert!(targets["dev"].stream);
        let command = RubyPlugin
            .with_files_list(
                "test",
                "bin/rails test",
                &[
                    PathBuf::from("app/models/user.rb"),
                    PathBuf::from("test/models/user_test.rb"),
                ],
            )
            .unwrap();
        assert_eq!(command, "bin/rails test test/models/user_test.rb");
    }

    #[test]
    fn skips_duplicate_and_fixture_gemspecs() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("a.gemspec");
        let second = tmp.path().join("b.gemspec");
        std::fs::write(&first, "").unwrap();
        std::fs::write(&second, "").unwrap();
        assert!(!RubyPlugin.should_skip(&first));
        assert!(RubyPlugin.should_skip(&second));

        let fixture = tmp.path().join("spec/fixtures/example/example.gemspec");
        std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        std::fs::write(&fixture, "").unwrap();
        assert!(RubyPlugin.should_skip(&fixture));
    }

    #[test]
    fn build_supports_strict_warnings_and_cache_boundaries() {
        assert_eq!(
            RubyPlugin.with_warnings_as_errors("build", "bundle exec gem build sample.gemspec"),
            Some("bundle exec gem build sample.gemspec --strict".to_string())
        );
        assert!(RubyPlugin.cache_inputs("deps").source_globs.is_empty());
        assert!(RubyPlugin
            .cache_inputs("test")
            .source_globs
            .contains(&"spec/**/*".to_string()));
    }

    #[test]
    fn ancestor_gemfile_disables_member_cache_to_avoid_stale_lockfile_results() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Gemfile"),
            "source 'https://rubygems.org'\n",
        )
        .unwrap();
        let member = tmp.path().join("gems/member");
        std::fs::create_dir_all(member.join("spec")).unwrap();
        std::fs::write(member.join(".rspec"), "").unwrap();
        let gemspec = member.join("member.gemspec");
        std::fs::write(&gemspec, "s.name = 'member'\n").unwrap();

        let targets = RubyPlugin
            .detect_targets(&context(&gemspec, tmp.path()))
            .unwrap();
        assert!(targets.values().all(|target| {
            target.cache.as_ref().and_then(|cache| cache.enabled) == Some(false)
        }));
    }

    #[test]
    fn gemspec_without_gemfile_uses_rubygems_and_rake_without_fake_deps_target() {
        let tmp = tempfile::tempdir().unwrap();
        let gemspec = tmp.path().join("standalone.gemspec");
        std::fs::write(&gemspec, "s.name = 'standalone'\n").unwrap();
        std::fs::create_dir(tmp.path().join("test")).unwrap();
        std::fs::write(tmp.path().join("Rakefile"), "Rake::TestTask.new\n").unwrap();

        let targets = RubyPlugin
            .detect_targets(&context(&gemspec, tmp.path()))
            .unwrap();
        assert!(!targets.contains_key("deps"));
        assert_eq!(targets["build"].command, "gem build standalone.gemspec");
        assert_eq!(targets["test"].command, "rake test");
        assert!(targets["build"].depends_on.is_empty());
    }
}
