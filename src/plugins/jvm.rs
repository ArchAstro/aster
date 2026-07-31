use ignore::WalkBuilder;
use std::path::Path;

/// Detect JVM source languages from source files beneath a project's `src`
/// directory. Build scripts such as `build.gradle.kts` intentionally do not
/// participate: Kotlin DSL is a build configuration language, not evidence
/// that the project itself contains Kotlin.
pub(super) fn source_languages(project_dir: &Path) -> Vec<String> {
    let source_root = project_dir.join("src");
    if !source_root.is_dir() {
        return Vec::new();
    }

    let mut java = false;
    let mut kotlin = false;
    for entry in WalkBuilder::new(source_root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        match entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("java") => java = true,
            // Kotlin scripts beneath src/ are project sources (for example,
            // Gradle precompiled script plugins). Top-level build.gradle.kts
            // is outside this walk and remains build-system metadata only.
            Some("kt" | "kts") => kotlin = true,
            _ => {}
        }
        if java && kotlin {
            break;
        }
    }

    let mut languages = Vec::new();
    if java {
        languages.push("java".to_string());
    }
    if kotlin {
        languages.push("kotlin".to_string());
    }
    languages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_jvm_sources_without_treating_build_scripts_as_project_language() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("build.gradle.kts"), "plugins { java }").unwrap();
        assert!(source_languages(temp.path()).is_empty());

        std::fs::create_dir_all(temp.path().join("src/test/kotlin")).unwrap();
        std::fs::write(temp.path().join("src/test/kotlin/setup.kts"), "").unwrap();
        assert_eq!(source_languages(temp.path()), vec!["kotlin"]);

        std::fs::create_dir_all(temp.path().join("src/main/java")).unwrap();
        std::fs::write(temp.path().join("src/main/java/App.java"), "class App {}").unwrap();
        std::fs::write(
            temp.path().join("src/test/kotlin/AppTest.kt"),
            "class AppTest",
        )
        .unwrap();

        assert_eq!(source_languages(temp.path()), vec!["java", "kotlin"]);
    }
}
