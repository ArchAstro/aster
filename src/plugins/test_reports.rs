//! Capturing per-test results from each language's test runner.
//!
//! Each function recognises its runner in a plain command, the same way
//! [`super::related_tests`] does, and returns how to capture a report from
//! it. A command the function does not recognise gets no plan and runs as
//! written. Plans keep the runner's console output unchanged: a reporter is
//! added alongside the one the user sees, never in place of it.

use std::path::Path;

use super::related_tests::Argv;
use crate::test_results::{
    Confidence, OutputRewrite, ReportContext, ReportFiles, ReportFormat, ReportPlan,
};

/// `pytest`, however it is launched (`python -m pytest`, `uv run pytest`).
///
/// Adds `--junitxml` pointing into the report directory, before any `--`.
/// A command that already writes a JUnit report keeps it, and Aster reads
/// that file instead.
pub(super) fn pytest(ctx: &ReportContext) -> Option<ReportPlan> {
    let argv = Argv::parse(ctx.command)?;
    let runner = argv.parts.iter().position(|p| {
        let name = Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        name == "pytest" || name == "py.test"
    })?;
    let plan = |command: String, file: std::path::PathBuf| ReportPlan {
        command,
        env: Vec::new(),
        files: ReportFiles::Paths(vec![file]),
        format: ReportFormat::JUnitXml,
        confidence: Confidence::Complete,
        rewrite: None,
    };

    let mut end = argv.parts.len();
    let mut i = runner + 1;
    while i < argv.parts.len() {
        let part = argv.parts[i].as_str();
        if part == "--" {
            end = i;
            break;
        }
        let existing = match part {
            "--junitxml" | "--junit-xml" => Some(argv.parts.get(i + 1)?.as_str()),
            _ => part
                .strip_prefix("--junitxml=")
                .or_else(|| part.strip_prefix("--junit-xml=")),
        };
        if let Some(path) = existing {
            return Some(plan(ctx.command.to_string(), ctx.working_dir.join(path)));
        }
        i += 1;
    }

    let file = ctx.report_dir.join("pytest.xml");
    let command = argv.render_inserting(end, &[format!("--junitxml={}", file.display())]);
    Some(plan(command, file))
}

/// `go test`: adds `-json` and renders the events back to `go test` text
/// for the log. A command that already asks for `-json` keeps its output
/// as written.
pub(super) fn go(ctx: &ReportContext) -> Option<ReportPlan> {
    let argv = Argv::parse(ctx.command)?;
    let parts = &argv.parts;
    if Path::new(&parts[argv.program]).file_name()?.to_str()? != "go"
        || parts.get(argv.program + 1).map(String::as_str) != Some("test")
    {
        return None;
    }
    // Flags after `-args` belong to the test binary, not to `go test`.
    let own = parts[argv.program + 2..]
        .iter()
        .take_while(|p| *p != "-args" && *p != "--args")
        .map(String::as_str)
        .collect::<Vec<_>>();
    let flag = |name: &str| {
        own.iter().any(|p| {
            let p = p
                .strip_prefix('-')
                .map(|p| p.strip_prefix('-').unwrap_or(p));
            p.is_some_and(|p| p == name || p == format!("{name}=true"))
        })
    };
    let plan = |command: String, rewrite| ReportPlan {
        command,
        env: Vec::new(),
        files: ReportFiles::Stdout,
        format: ReportFormat::GoTestJson,
        confidence: Confidence::Complete,
        rewrite,
    };
    if flag("json") {
        return Some(plan(ctx.command.to_string(), None));
    }
    let command = argv.render_inserting(argv.program + 2, &["-json".to_string()]);
    Some(plan(
        command,
        Some(OutputRewrite::GoTestJson { verbose: flag("v") }),
    ))
}

/// Gradle's `test` target: Gradle already writes JUnit XML under
/// `build/test-results`, so the command is unchanged.
pub(super) fn gradle(ctx: &ReportContext) -> Option<ReportPlan> {
    (ctx.target_name == "test").then(|| ReportPlan {
        command: ctx.command.to_string(),
        env: Vec::new(),
        files: ReportFiles::Globs {
            root: ctx.project_dir.to_path_buf(),
            patterns: vec!["build/test-results/**/*.xml".to_string()],
        },
        format: ReportFormat::JUnitXml,
        confidence: Confidence::Complete,
        rewrite: None,
    })
}

/// Maven's `test` target: Surefire and Failsafe already write
/// `TEST-*.xml`, so the command is unchanged.
pub(super) fn maven(ctx: &ReportContext) -> Option<ReportPlan> {
    (ctx.target_name == "test").then(|| ReportPlan {
        command: ctx.command.to_string(),
        env: Vec::new(),
        files: ReportFiles::Globs {
            root: ctx.project_dir.to_path_buf(),
            patterns: vec![
                "target/surefire-reports/TEST-*.xml".to_string(),
                "target/failsafe-reports/TEST-*.xml".to_string(),
            ],
        },
        format: ReportFormat::JUnitXml,
        confidence: Confidence::Complete,
        rewrite: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture {
        project: PathBuf,
        report_dir: PathBuf,
    }

    fn fixture() -> Fixture {
        Fixture {
            project: PathBuf::from("/work/app"),
            report_dir: PathBuf::from("/tmp/aster-reports/1"),
        }
    }

    fn context<'a>(f: &'a Fixture, target: &'a str, command: &'a str) -> ReportContext<'a> {
        ReportContext {
            project_dir: &f.project,
            working_dir: &f.project,
            target_name: target,
            command,
            report_dir: &f.report_dir,
        }
    }

    #[test]
    fn pytest_adds_a_junit_report() {
        let f = fixture();
        for (command, expected) in [
            (
                "pytest",
                "pytest --junitxml=/tmp/aster-reports/1/pytest.xml",
            ),
            (
                "uv run pytest -q tests",
                "uv run pytest -q tests --junitxml=/tmp/aster-reports/1/pytest.xml",
            ),
            (
                "PYTHONPATH=src python -m pytest -x",
                "PYTHONPATH=src python -m pytest -x --junitxml=/tmp/aster-reports/1/pytest.xml",
            ),
            (
                "pytest -k fast -- tests/a.py",
                "pytest -k fast --junitxml=/tmp/aster-reports/1/pytest.xml -- tests/a.py",
            ),
        ] {
            let plan = pytest(&context(&f, "test", command)).unwrap();
            assert_eq!(plan.command, expected);
            assert_eq!(
                plan.files,
                ReportFiles::Paths(vec![f.report_dir.join("pytest.xml")])
            );
            assert!(plan.env.is_empty());
        }
    }

    #[test]
    fn pytest_keeps_an_existing_junit_report() {
        let f = fixture();
        for command in [
            "pytest --junitxml out/report.xml",
            "pytest --junitxml=out/report.xml",
            "pytest --junit-xml=out/report.xml -q",
        ] {
            let plan = pytest(&context(&f, "test", command)).unwrap();
            assert_eq!(plan.command, command);
            assert_eq!(
                plan.files,
                ReportFiles::Paths(vec![f.project.join("out/report.xml")])
            );
        }
    }

    #[test]
    fn pytest_ignores_other_commands() {
        let f = fixture();
        for command in [
            "ruff check .",
            "pytest && echo done",
            "bash -c 'pytest'",
            "python -m unittest",
        ] {
            assert!(pytest(&context(&f, "test", command)).is_none(), "{command}");
        }
    }

    #[test]
    fn pytest_quotes_a_report_path_with_spaces() {
        let f = Fixture {
            project: PathBuf::from("/work/app"),
            report_dir: PathBuf::from("/tmp/my reports"),
        };
        let plan = pytest(&context(&f, "test", "pytest")).unwrap();
        assert_eq!(
            shell_words::split(&plan.command).unwrap(),
            vec!["pytest", "--junitxml=/tmp/my reports/pytest.xml"]
        );
    }

    #[test]
    fn go_adds_json_and_renders_it_back() {
        let f = fixture();
        for (command, expected, verbose) in [
            ("go test ./...", "go test -json ./...", false),
            (
                "CGO_ENABLED=0 go test -race -v ./pkg/...",
                "CGO_ENABLED=0 go test -json -race -v ./pkg/...",
                true,
            ),
            (
                "go test -run '^(TestA|TestB)$' ./a",
                "go test -json -run '^(TestA|TestB)$' ./a",
                false,
            ),
            ("go test ./a -args -v", "go test -json ./a -args -v", false),
        ] {
            let plan = go(&context(&f, "test", command)).unwrap();
            assert_eq!(plan.command, expected);
            assert_eq!(plan.files, ReportFiles::Stdout);
            assert_eq!(plan.rewrite, Some(OutputRewrite::GoTestJson { verbose }));
        }
    }

    #[test]
    fn go_keeps_a_command_that_already_asks_for_json() {
        let f = fixture();
        let plan = go(&context(&f, "test", "go test -json ./...")).unwrap();
        assert_eq!(plan.command, "go test -json ./...");
        assert_eq!(plan.rewrite, None);
        assert_eq!(plan.files, ReportFiles::Stdout);
    }

    #[test]
    fn go_ignores_other_commands() {
        let f = fixture();
        for command in ["go vet ./...", "go test ./... | tee out", "gotestsum ./..."] {
            assert!(go(&context(&f, "test", command)).is_none(), "{command}");
        }
    }

    #[test]
    fn gradle_and_maven_read_their_own_reports_for_test_only() {
        let f = fixture();
        let plan = gradle(&context(&f, "test", "./gradlew :api:test")).unwrap();
        assert_eq!(plan.command, "./gradlew :api:test");
        assert_eq!(
            plan.files,
            ReportFiles::Globs {
                root: f.project.clone(),
                patterns: vec!["build/test-results/**/*.xml".into()]
            }
        );
        // `check` runs tests too, but its results belong to the test target.
        assert!(gradle(&context(&f, "lint", "./gradlew :api:check")).is_none());

        let plan = maven(&context(&f, "test", "./mvnw -pl :api -am test")).unwrap();
        assert_eq!(plan.command, "./mvnw -pl :api -am test");
        assert!(maven(&context(&f, "build", "./mvnw package")).is_none());
    }
}
