//! Per-test results captured from a target's native test runner.
//!
//! A language plugin describes how to capture results from one test command
//! with a [`ReportPlan`]: the command to run instead (usually the original
//! with a reporter flag added), extra environment, where the runner writes
//! its report, and the report's format. The executor runs the planned
//! command, then [`collect`] reads the report into [`TestCase`]s.
//!
//! The plan is applied after the cache key is computed from the original
//! command, so capturing results never changes what is cached.

mod context;
mod gojson;
mod junit;
mod stream;

pub use context::detect_run;
pub use stream::{default_path, records, write};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use walkdir::WalkDir;

pub use aster_results_model::{Confidence, TestOutcome};

/// One attempt of one test, as the runner reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct TestCase {
    /// Path relative to the project directory, when the runner names it.
    pub file: Option<String>,
    /// Module, class or describe block.
    pub suite: Option<String>,
    pub name: String,
    pub outcome: TestOutcome,
    /// 1 for the first attempt.
    pub attempt: u32,
    pub duration_ms: Option<u64>,
    /// Failure or error text, unscrubbed.
    pub message: Option<String>,
}

/// Every test a target ran, with how they were captured.
#[derive(Debug, Clone, PartialEq)]
pub struct TestReport {
    /// The capture strategy, e.g. `junit`.
    pub capture: String,
    pub confidence: Confidence,
    pub cases: Vec<TestCase>,
}

impl TestReport {
    /// Number of attempts that failed or errored.
    pub fn failures(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.outcome.is_failure())
            .count()
    }
}

/// A report format Aster can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    /// JUnit XML: pytest, Gradle, Maven Surefire/Failsafe, vitest, bun,
    /// nextest.
    JUnitXml,
    /// `go test -json` events.
    GoTestJson,
}

impl ReportFormat {
    /// The capture name recorded with the results.
    pub fn capture_name(self) -> &'static str {
        match self {
            ReportFormat::JUnitXml => "junit",
            ReportFormat::GoTestJson => "go-json",
        }
    }
}

/// Where a runner leaves its report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportFiles {
    /// These exact files. Missing files are skipped.
    Paths(Vec<PathBuf>),
    /// Files under `root` matching any pattern (globs relative to `root`).
    Globs {
        root: PathBuf,
        patterns: Vec<String>,
    },
    /// The command's own console output.
    Stdout,
}

/// How to turn a command's console output back into what the user expects
/// to see, when the reporter replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRewrite {
    /// Render `go test -json` events as `go test` text; `verbose` when the
    /// command asked for `-v`.
    GoTestJson { verbose: bool },
}

/// Rewrite a command's console output for its log.
pub fn rewrite_output(rewrite: OutputRewrite, output: &str) -> String {
    match rewrite {
        OutputRewrite::GoTestJson { verbose } => gojson::render(output, verbose),
    }
}

/// How to capture structured results from one test command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportPlan {
    /// The command to run in place of the original.
    pub command: String,
    /// Environment added for the command.
    pub env: Vec<(String, String)>,
    pub files: ReportFiles,
    pub format: ReportFormat,
    pub confidence: Confidence,
    /// Set when the planned command's console output is the report, so the
    /// log shows a rendering of it instead.
    pub rewrite: Option<OutputRewrite>,
}

/// What a plugin is told when asked for a [`ReportPlan`].
#[derive(Debug, Clone, Copy)]
pub struct ReportContext<'a> {
    /// Absolute project directory.
    pub project_dir: &'a Path,
    /// Directory the command runs in.
    pub working_dir: &'a Path,
    pub target_name: &'a str,
    pub command: &'a str,
    /// An empty directory outside the checkout that the runner may write
    /// its report into. Removed after the results are read.
    pub report_dir: &'a Path,
}

/// A fresh, empty directory for one command's report.
///
/// It lives under `$RUNNER_TEMP` when set (GitHub Actions), else the system
/// temp directory, never in the checkout: some repositories lint every file
/// under it.
pub fn new_report_dir() -> Result<PathBuf> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let base = std::env::var_os("RUNNER_TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = base.join("aster-reports").join(format!(
        "{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create report directory {}", dir.display()))?;
    Ok(dir)
}

/// The tests read from one command's report.
#[derive(Debug, Clone, PartialEq)]
pub struct Collected {
    pub cases: Vec<TestCase>,
    /// False when the report itself shows tests may be missing, for example
    /// a package that failed to build.
    pub complete: bool,
}

/// Read the report a planned command produced. `stdout` is the command's
/// raw console output, read when the plan's report is
/// [`ReportFiles::Stdout`].
///
/// Only files modified at or after `started` count, so a report left by an
/// earlier run is never read as this run's. An error means the results
/// could not be read; no report file at all is an empty result.
pub fn collect(plan: &ReportPlan, started: SystemTime, stdout: Option<&str>) -> Result<Collected> {
    if plan.files == ReportFiles::Stdout {
        let output = stdout.unwrap_or("");
        return parse(plan.format, output).context("cannot parse the test output");
    }
    let mut collected = Collected {
        cases: Vec::new(),
        complete: true,
    };
    for path in report_files(&plan.files, started)? {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read test report {}", path.display()))?;
        let parsed = parse(plan.format, &content)
            .with_context(|| format!("cannot parse test report {}", path.display()))?;
        collected.cases.extend(parsed.cases);
        collected.complete &= parsed.complete;
    }
    Ok(collected)
}

fn parse(format: ReportFormat, content: &str) -> Result<Collected> {
    Ok(match format {
        ReportFormat::JUnitXml => Collected {
            cases: junit::parse(content)?,
            complete: true,
        },
        ReportFormat::GoTestJson => {
            let parsed = gojson::parse(content);
            Collected {
                cases: parsed.cases,
                complete: parsed.complete,
            }
        }
    })
}

fn report_files(files: &ReportFiles, started: SystemTime) -> Result<Vec<PathBuf>> {
    // File systems with coarse timestamps can stamp a file written just
    // after `started` with an earlier time.
    let since = started
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or(started);
    let fresh = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified >= since)
    };
    match files {
        ReportFiles::Stdout => Ok(Vec::new()),
        ReportFiles::Paths(paths) => Ok(paths
            .iter()
            .filter(|path| path.is_file() && fresh(path))
            .cloned()
            .collect()),
        ReportFiles::Globs { root, patterns } => {
            let mut builder = GlobSetBuilder::new();
            for pattern in patterns {
                builder.add(Glob::new(pattern)?);
            }
            let set = builder.build()?;
            let mut found: Vec<PathBuf> = WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| entry.into_path())
                .filter(|path| {
                    path.strip_prefix(root)
                        .is_ok_and(|relative| set.is_match(relative))
                })
                .filter(|path| fresh(path))
                .collect();
            found.sort();
            Ok(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    const REPORT: &str =
        r#"<testsuite name="s"><testcase classname="A" name="one" time="0.5"/></testsuite>"#;

    fn plan(files: ReportFiles) -> ReportPlan {
        ReportPlan {
            command: "true".into(),
            env: Vec::new(),
            files,
            format: ReportFormat::JUnitXml,
            confidence: Confidence::Complete,
            rewrite: None,
        }
    }

    #[test]
    fn report_dirs_are_fresh_and_distinct() {
        let a = new_report_dir().unwrap();
        let b = new_report_dir().unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir());
        assert_eq!(std::fs::read_dir(&a).unwrap().count(), 0);
        std::fs::remove_dir_all(a).unwrap();
        std::fs::remove_dir_all(b).unwrap();
    }

    #[test]
    fn collects_matching_globs() {
        let tmp = TempDir::new().unwrap();
        let reports = tmp.path().join("build/test-results/test");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(reports.join("TEST-A.xml"), REPORT).unwrap();
        std::fs::write(reports.join("binary.bin"), "not xml").unwrap();
        let cases = collect(
            &plan(ReportFiles::Globs {
                root: tmp.path().to_path_buf(),
                patterns: vec!["build/test-results/**/*.xml".into()],
            }),
            SystemTime::now() - Duration::from_secs(10),
            None,
        )
        .unwrap()
        .cases;
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "one");
        assert_eq!(cases[0].duration_ms, Some(500));
    }

    #[test]
    fn ignores_reports_from_before_the_run() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("old.xml");
        std::fs::write(&path, REPORT).unwrap();
        let cases = collect(
            &plan(ReportFiles::Paths(vec![path])),
            SystemTime::now() + Duration::from_secs(60),
            None,
        )
        .unwrap()
        .cases;
        assert!(cases.is_empty());
    }

    #[test]
    fn missing_report_is_empty_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let cases = collect(
            &plan(ReportFiles::Paths(vec![tmp.path().join("none.xml")])),
            SystemTime::now(),
            None,
        )
        .unwrap()
        .cases;
        assert!(cases.is_empty());
    }

    #[test]
    fn unreadable_report_is_an_error_naming_the_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.xml");
        std::fs::write(&path, "<testsuite><testcase").unwrap();
        let error = collect(
            &plan(ReportFiles::Paths(vec![path])),
            SystemTime::now() - Duration::from_secs(10),
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("bad.xml"), "{error:#}");
    }

    #[test]
    fn reads_a_report_from_stdout() {
        let mut plan = plan(ReportFiles::Stdout);
        plan.format = ReportFormat::GoTestJson;
        let stdout = r#"{"Action":"pass","Package":"p","Test":"TestA","Elapsed":0.5}"#;
        let collected = collect(&plan, SystemTime::now(), Some(stdout)).unwrap();
        assert!(collected.complete);
        assert_eq!(collected.cases[0].name, "TestA");
        assert_eq!(collected.cases[0].duration_ms, Some(500));
    }

    #[test]
    fn counts_failures() {
        let report = TestReport {
            capture: "junit".into(),
            confidence: Confidence::Complete,
            cases: vec![
                TestCase {
                    file: None,
                    suite: None,
                    name: "a".into(),
                    outcome: TestOutcome::Fail,
                    attempt: 1,
                    duration_ms: None,
                    message: None,
                },
                TestCase {
                    file: None,
                    suite: None,
                    name: "b".into(),
                    outcome: TestOutcome::Pass,
                    attempt: 1,
                    duration_ms: None,
                    message: None,
                },
            ],
        };
        assert_eq!(report.failures(), 1);
    }
}
