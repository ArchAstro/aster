//! Capturing per-test results from each language's test runner.
//!
//! Each function recognises its runner in a plain command, the same way
//! [`super::related_tests`] does, and returns how to capture a report from
//! it. A command the function does not recognise gets no plan and runs as
//! written. Plans keep the runner's console output unchanged: a reporter is
//! added alongside the one the user sees, never in place of it.

use std::path::Path;

use super::related_tests::{js_runner, js_script, Argv, Delegated};
use crate::test_results::exunit;
use crate::test_results::{
    Confidence, OutputRewrite, ReportContext, ReportFiles, ReportFormat, ReportPlan,
};

/// Plan for a test command whatever the project's language: the runners
/// that are recognised by their command alone (`mix test`, `go test`,
/// `pytest`), directly or inside a shell one-liner. A shard project, for
/// example, is often a bare `package.json` whose target runs another
/// project's `mix test`.
pub(crate) fn recognise(ctx: &ReportContext) -> Option<ReportPlan> {
    delegating(ctx, mix)
        .or_else(|| delegating(ctx, go))
        .or_else(|| delegating(ctx, pytest))
        .or_else(|| delegating(ctx, js))
        .or_else(|| delegating(ctx, cargo))
}

/// `cargo test` and `cargo nextest run`.
///
/// `cargo test` is read from its console output, which is always partial.
/// nextest writes JUnit only when the repository's nextest config asks for
/// it, under `target/nextest/<profile>/`; Aster reads that file when it is
/// there, and with no file the results are partial too.
pub(super) fn cargo(ctx: &ReportContext) -> Option<ReportPlan> {
    let argv = Argv::parse(ctx.command)?;
    let parts = &argv.parts[argv.program..];
    if std::path::Path::new(&parts[0]).file_name()?.to_str()? != "cargo" {
        return None;
    }
    // Skip toolchain overrides and flags before the subcommand.
    let rest: Vec<&str> = parts[1..]
        .iter()
        .map(String::as_str)
        .skip_while(|p| p.starts_with('+') || p.starts_with('-'))
        .collect();
    let base = ReportPlan {
        command: ctx.command.to_string(),
        env: Vec::new(),
        files: ReportFiles::Stdout,
        format: ReportFormat::LibtestText,
        confidence: Confidence::Partial,
        rewrite: None,
        relative_to: None,
    };
    match rest.as_slice() {
        ["test", ..] => Some(base),
        ["nextest", "run", ..] => {
            let target = std::env::var_os("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .map(|dir| ctx.working_dir.join(dir))
                .unwrap_or_else(|| ctx.working_dir.join("target"));
            Some(ReportPlan {
                files: ReportFiles::Globs {
                    root: target,
                    patterns: vec!["nextest/*/*.xml".to_string()],
                },
                format: ReportFormat::JUnitXml,
                confidence: Confidence::Complete,
                ..base
            })
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsRunner {
    Vitest,
    Jest,
    Bun,
}

/// Vitest, Jest and `bun test`, run directly (`vitest run`, `npx jest`) or
/// through a package script that is nothing but the runner (`pnpm test`
/// where `test` is `vitest run`).
///
/// - Vitest gets a JUnit reporter next to the reporters it would use. CLI
///   reporters replace the config's, so a config that sets `reporters`
///   leaves the command alone; otherwise the defaults Vitest would pick
///   (`default`, plus `github-actions` under GitHub Actions) are named
///   explicitly.
/// - Jest gets `--json --outputFile`, which leaves its console reporter as
///   it was.
/// - `bun test` gets `--reporter=junit --reporter-outfile`.
pub(super) fn js(ctx: &ReportContext) -> Option<ReportPlan> {
    let argv = Argv::parse(ctx.command)?;
    if let Some(start) = js_runner(&argv) {
        let runner = js_runner_kind(&argv, start)?;
        let (flags, plan) = js_flags(ctx, runner, &argv, start)?;
        return Some(ReportPlan {
            command: argv.render_inserting(start, &flags),
            ..plan
        });
    }

    let script = js_script(&argv)?;
    let launcher = std::path::Path::new(&argv.parts[argv.program])
        .file_name()?
        .to_str()?
        .to_string();
    let manifest = std::fs::read_to_string(ctx.working_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let body = manifest.get("scripts")?.get(script)?.as_str()?;
    let script_argv = Argv::parse(body)?;
    let start = js_runner(&script_argv)?;
    let runner = js_runner_kind(&script_argv, start)?;
    let (flags, plan) = js_flags(ctx, runner, &script_argv, start)?;
    // npm passes a script its arguments only after `--`; pnpm, yarn and
    // bun pass them as they are, and would hand a `--` to the runner.
    let mut add = Vec::new();
    if launcher == "npm" {
        add.push("--".to_string());
    }
    add.extend(flags);
    Some(ReportPlan {
        command: argv.render_inserting(usize::MAX, &add),
        ..plan
    })
}

fn js_runner_kind(argv: &Argv, start: usize) -> Option<JsRunner> {
    argv.parts[argv.program..start].iter().find_map(|part| {
        match std::path::Path::new(part).file_name()?.to_str()? {
            "vitest" => Some(JsRunner::Vitest),
            "jest" => Some(JsRunner::Jest),
            "bun" => Some(JsRunner::Bun),
            _ => None,
        }
    })
}

/// The flags that add a report, and the plan for reading it (its command
/// is filled in by the caller). `None` when the user already chose where
/// that report goes, or the runner's config would lose its reporters.
fn js_flags(
    ctx: &ReportContext,
    runner: JsRunner,
    argv: &Argv,
    start: usize,
) -> Option<(Vec<String>, ReportPlan)> {
    let args = &argv.parts[start..];
    let has = |flag: &str| {
        args.iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    let plan = |file: std::path::PathBuf, format| ReportPlan {
        command: String::new(),
        env: Vec::new(),
        files: ReportFiles::Paths(vec![file]),
        format,
        confidence: Confidence::Complete,
        rewrite: None,
        relative_to: Some(ctx.working_dir.to_path_buf()),
    };
    match runner {
        JsRunner::Vitest => {
            let file = ctx.report_dir.join("vitest.xml");
            let mut flags = Vec::new();
            if !has("--reporter") {
                if vitest_config_sets_reporters(ctx.working_dir, argv, start) {
                    return None;
                }
                flags.push("--reporter=default".to_string());
                if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                    flags.push("--reporter=github-actions".to_string());
                }
            }
            flags.push("--reporter=junit".to_string());
            flags.push(format!("--outputFile.junit={}", file.display()));
            Some((flags, plan(file, ReportFormat::JUnitXml)))
        }
        JsRunner::Jest => {
            if has("--json") || has("--outputFile") {
                return None;
            }
            let file = ctx.report_dir.join("jest.json");
            let flags = vec![
                "--json".to_string(),
                format!("--outputFile={}", file.display()),
            ];
            Some((flags, plan(file, ReportFormat::JestJson)))
        }
        JsRunner::Bun => {
            if has("--reporter") || has("--reporter-outfile") {
                return None;
            }
            let file = ctx.report_dir.join("bun.xml");
            let flags = vec![
                "--reporter=junit".to_string(),
                format!("--reporter-outfile={}", file.display()),
            ];
            Some((flags, plan(file, ReportFormat::JUnitXml)))
        }
    }
}

/// Whether the Vitest config the command uses lists `reporters`.
fn vitest_config_sets_reporters(dir: &Path, argv: &Argv, start: usize) -> bool {
    let args = &argv.parts[start..];
    let named = args.iter().enumerate().find_map(|(i, a)| {
        if a == "--config" || a == "-c" {
            args.get(i + 1).cloned()
        } else {
            a.strip_prefix("--config=").map(str::to_string)
        }
    });
    let candidates: Vec<std::path::PathBuf> = match named {
        Some(config) => vec![dir.join(config)],
        None => ["vitest", "vite"]
            .iter()
            .flat_map(|stem| {
                ["ts", "mts", "cts", "js", "mjs", "cjs"]
                    .iter()
                    .map(move |ext| dir.join(format!("{stem}.config.{ext}")))
            })
            .collect(),
    };
    candidates
        .iter()
        .any(|path| std::fs::read_to_string(path).is_ok_and(|c| c.contains("reporters")))
}

/// Plan for a command, seeing through a shell one-liner that ends in the
/// test command, such as a shard project's
/// `bash -c 'cd ../platform && MIX_TEST_PARTITION=1 exec mix test'`.
///
/// The inner command is planned as if it ran in the directory it `cd`s to,
/// and the plan's rewrite is put back into the one-liner.
pub(super) fn delegating(
    ctx: &ReportContext,
    plan: impl Fn(&ReportContext) -> Option<ReportPlan>,
) -> Option<ReportPlan> {
    if let Some(found) = plan(ctx) {
        return Some(found);
    }
    let delegated = Delegated::parse(ctx.command)?;
    let working_dir = match &delegated.dir {
        Some(dir) => ctx.working_dir.join(dir),
        None => ctx.working_dir.to_path_buf(),
    };
    let inner = ReportContext {
        working_dir: &working_dir,
        command: &delegated.command,
        ..*ctx
    };
    let mut found = plan(&inner)?;
    found.command = delegated.render(&found.command);
    Some(found)
}

/// `mix test`, including `mix do --app name test`.
///
/// Loads Aster's formatter with `elixir -r` and adds it next to the
/// formatters the run would use, so the console output is unchanged. A
/// project that configures its own formatters (`test_helper.exs` or
/// `config/`) would lose them to a `--formatter` flag, so its command is
/// left alone and only the failures in its console output are read.
pub(super) fn mix(ctx: &ReportContext) -> Option<ReportPlan> {
    let argv = Argv::parse(ctx.command)?;
    // Only a bare `mix` from PATH: `elixir -S mix` finds the same script.
    if argv.parts[argv.program] != "mix" {
        return None;
    }
    let test = argv.program
        + 1
        + argv.parts[argv.program + 1..]
            .iter()
            .position(|p| p == "test")?;
    // `mix do a, b` runs several tasks.
    if argv.parts.iter().any(|p| p.ends_with(',')) {
        return None;
    }
    let listed_formatter = argv.parts[test + 1..]
        .iter()
        .any(|p| p == "--formatter" || p.starts_with("--formatter="));
    if !listed_formatter && configures_formatters(ctx.working_dir) {
        return Some(ReportPlan {
            command: ctx.command.to_string(),
            env: Vec::new(),
            files: ReportFiles::Stdout,
            format: ReportFormat::ExUnit,
            confidence: Confidence::Partial,
            rewrite: None,
            relative_to: None,
        });
    }

    let formatter = ctx.report_dir.join("aster_exunit_formatter.exs");
    std::fs::write(&formatter, exunit::FORMATTER_SOURCE).ok()?;
    let report = ctx.report_dir.join("exunit.ndjson");

    let mut parts: Vec<String> = argv.parts[..argv.program].to_vec();
    parts.extend([
        "elixir".to_string(),
        "-r".to_string(),
        formatter.display().to_string(),
        "-S".to_string(),
    ]);
    parts.extend(argv.parts[argv.program..=test].iter().cloned());
    parts.extend([
        "--formatter".to_string(),
        exunit::FORMATTER_MODULE.to_string(),
    ]);
    if !listed_formatter {
        parts.extend(["--formatter".to_string(), "ExUnit.CLIFormatter".to_string()]);
    }
    parts.extend(argv.parts[test + 1..].iter().cloned());
    let rewritten = Argv {
        program: argv.program,
        parts,
    };
    Some(ReportPlan {
        command: rewritten.render_inserting(usize::MAX, &[]),
        env: vec![(exunit::REPORT_ENV.to_string(), report.display().to_string())],
        files: ReportFiles::Paths(vec![report]),
        format: ReportFormat::ExUnit,
        confidence: Confidence::Complete,
        rewrite: None,
        relative_to: None,
    })
}

/// Whether the Mix project in `dir` sets ExUnit formatters itself.
fn configures_formatters(dir: &Path) -> bool {
    let mentions = |path: &Path| {
        std::fs::read_to_string(path).is_ok_and(|content| content.contains("formatters"))
    };
    let mut files = vec![
        dir.join("test/test_helper.exs"),
        dir.join("config/config.exs"),
        dir.join("config/test.exs"),
    ];
    if let Ok(apps) = std::fs::read_dir(dir.join("apps")) {
        files.extend(
            apps.filter_map(|app| app.ok())
                .map(|app| app.path().join("test/test_helper.exs")),
        );
    }
    files.iter().any(|path| mentions(path))
}

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
        relative_to: None,
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
        relative_to: None,
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
        relative_to: None,
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
        relative_to: None,
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
    fn mix_loads_the_formatter_next_to_the_console_one() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fixture {
            project: tmp.path().to_path_buf(),
            report_dir: tmp.path().join("reports"),
        };
        std::fs::create_dir_all(&f.report_dir).unwrap();
        let formatter = f.report_dir.join("aster_exunit_formatter.exs");
        let report = f.report_dir.join("exunit.ndjson");
        let plan = mix(&context(
            &f,
            "test",
            "MIX_ENV=test mix test --warnings-as-errors",
        ))
        .unwrap();
        assert_eq!(
            shell_words::split(&plan.command).unwrap(),
            vec![
                "MIX_ENV=test".to_string(),
                "elixir".into(),
                "-r".into(),
                formatter.display().to_string(),
                "-S".into(),
                "mix".into(),
                "test".into(),
                "--formatter".into(),
                "AsterTestResults.ExUnitFormatter".into(),
                "--formatter".into(),
                "ExUnit.CLIFormatter".into(),
                "--warnings-as-errors".into(),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&formatter).unwrap(),
            exunit::FORMATTER_SOURCE
        );
        assert_eq!(
            plan.env,
            vec![(
                "ASTER_EXUNIT_REPORT".to_string(),
                report.display().to_string()
            )]
        );
        assert_eq!(plan.files, ReportFiles::Paths(vec![report]));
        assert_eq!(plan.confidence, Confidence::Complete);

        // A user's own formatter list is kept and joined, not extended with
        // the console formatter.
        let plan = mix(&context(&f, "test", "mix test --formatter JUnitFormatter")).unwrap();
        let parts = shell_words::split(&plan.command).unwrap();
        assert_eq!(parts.iter().filter(|p| *p == "--formatter").count(), 2);
        assert!(!parts.contains(&"ExUnit.CLIFormatter".to_string()));
    }

    #[test]
    fn mix_reads_the_console_when_the_project_sets_formatters() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(
            tmp.path().join("test/test_helper.exs"),
            "ExUnit.start(formatters: [ExUnit.CLIFormatter, My.Formatter])\n",
        )
        .unwrap();
        let f = Fixture {
            project: tmp.path().to_path_buf(),
            report_dir: tmp.path().join("reports"),
        };
        let plan = mix(&context(&f, "test", "mix test")).unwrap();
        assert_eq!(plan.command, "mix test");
        assert_eq!(plan.files, ReportFiles::Stdout);
        assert_eq!(plan.confidence, Confidence::Partial);
    }

    #[test]
    fn mix_ignores_other_tasks_and_paths() {
        let f = fixture();
        for command in [
            "mix compile",
            "mix test.canonical 2",
            "./bin/mix test",
            "mix do compile, test",
        ] {
            assert!(mix(&context(&f, "test", command)).is_none(), "{command}");
        }
    }

    #[test]
    fn delegating_plans_the_test_command_inside_a_shell_one_liner() {
        let tmp = tempfile::tempdir().unwrap();
        let shard = tmp.path().join("platform-test-shard-1");
        let platform = tmp.path().join("platform");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::create_dir_all(platform.join("test")).unwrap();
        std::fs::write(platform.join("test/test_helper.exs"), "ExUnit.start()\n").unwrap();
        let report_dir = tmp.path().join("reports");
        std::fs::create_dir_all(&report_dir).unwrap();
        let command =
            "bash -c 'cd ../platform && MIX_TEST_PARTITION=1 exec mix test --partitions 4'";
        let ctx = ReportContext {
            project_dir: &shard,
            working_dir: &shard,
            target_name: "test-ci",
            command,
            report_dir: &report_dir,
        };
        let plan = delegating(&ctx, mix).unwrap();
        let parts = shell_words::split(&plan.command).unwrap();
        assert_eq!(&parts[..2], &["bash", "-c"]);
        let script = &parts[2];
        assert!(
            script.starts_with("cd ../platform && MIX_TEST_PARTITION=1 exec elixir -r "),
            "{script}"
        );
        assert!(script.ends_with(
            "-S mix test --formatter AsterTestResults.ExUnitFormatter --formatter ExUnit.CLIFormatter --partitions 4"
        ));
        assert_eq!(plan.confidence, Confidence::Complete);

        // The project the shell `cd`s to decides whether formatters are set.
        std::fs::write(
            platform.join("test/test_helper.exs"),
            "ExUnit.start(formatters: [X])\n",
        )
        .unwrap();
        let plan = delegating(&ctx, mix).unwrap();
        assert_eq!(plan.command, command);
        assert_eq!(plan.files, ReportFiles::Stdout);
    }

    fn js_fixture(manifest: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, Fixture) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), manifest).unwrap();
        for (path, content) in files {
            std::fs::write(tmp.path().join(path), content).unwrap();
        }
        let f = Fixture {
            project: tmp.path().to_path_buf(),
            report_dir: PathBuf::from("/tmp/aster-reports/1"),
        };
        (tmp, f)
    }

    #[test]
    fn vitest_adds_junit_next_to_its_default_reporters() {
        let (_tmp, f) = js_fixture("{}", &[]);
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        let github = if ci { " --reporter=github-actions" } else { "" };
        let plan = js(&context(&f, "test", "npx vitest run src")).unwrap();
        assert_eq!(
            plan.command,
            format!(
                "npx vitest run --reporter=default{github} --reporter=junit \
                 --outputFile.junit=/tmp/aster-reports/1/vitest.xml src"
            )
        );
        assert_eq!(plan.format, ReportFormat::JUnitXml);
        assert_eq!(plan.relative_to.as_deref(), Some(f.project.as_path()));

        // Reporters the user names are kept; only junit is added.
        let plan = js(&context(&f, "test", "vitest run --reporter=dot")).unwrap();
        assert_eq!(
            plan.command,
            "vitest run --reporter=junit --outputFile.junit=/tmp/aster-reports/1/vitest.xml --reporter=dot"
        );
    }

    #[test]
    fn vitest_with_configured_reporters_is_left_alone() {
        let (_tmp, f) = js_fixture(
            "{}",
            &[(
                "vitest.config.ts",
                "export default { test: { reporters: ['verbose'] } }",
            )],
        );
        assert!(js(&context(&f, "test", "vitest run")).is_none());
        // Named reporters replace the config's anyway, so junit can join them.
        assert!(js(&context(&f, "test", "vitest run --reporter verbose")).is_some());
    }

    #[test]
    fn package_scripts_pass_the_flags_the_way_each_launcher_needs() {
        let (_tmp, f) = js_fixture(
            r#"{"scripts":{"test":"vitest run","test-ci":"jest --ci","bun-test":"bun test","build-test":"tsc && vitest run"}}"#,
            &[],
        );
        let plan = js(&context(&f, "test", "pnpm test")).unwrap();
        assert!(
            plan.command.starts_with("pnpm test --reporter=default"),
            "{}",
            plan.command
        );
        let plan = js(&context(&f, "test", "npm test")).unwrap();
        assert!(
            plan.command.starts_with("npm test -- --reporter=default"),
            "{}",
            plan.command
        );
        let plan = js(&context(&f, "test", "pnpm run test-ci")).unwrap();
        assert_eq!(
            plan.command,
            "pnpm run test-ci --json --outputFile=/tmp/aster-reports/1/jest.json"
        );
        assert_eq!(plan.format, ReportFormat::JestJson);
        let plan = js(&context(&f, "test", "yarn run bun-test")).unwrap();
        assert_eq!(
            plan.command,
            "yarn run bun-test --reporter=junit --reporter-outfile=/tmp/aster-reports/1/bun.xml"
        );
        // A script that is more than the runner gets no flags.
        assert!(js(&context(&f, "test", "pnpm run build-test")).is_none());
    }

    #[test]
    fn js_leaves_reports_the_user_already_writes_alone() {
        let (_tmp, f) = js_fixture("{}", &[]);
        for command in [
            "jest --json",
            "npx jest --outputFile=out.json",
            "bun test --reporter=junit --reporter-outfile=x.xml",
            "mocha",
            "node --test",
        ] {
            assert!(js(&context(&f, "test", command)).is_none(), "{command}");
        }
    }

    #[test]
    fn cargo_reads_test_output_and_nextest_junit() {
        let f = fixture();
        for command in [
            "cargo test",
            "cargo +nightly test --workspace",
            "RUST_BACKTRACE=1 cargo test -p aster",
        ] {
            let plan = cargo(&context(&f, "test", command)).unwrap();
            assert_eq!(plan.command, command);
            assert_eq!(plan.files, ReportFiles::Stdout);
            assert_eq!(plan.format, ReportFormat::LibtestText);
            assert_eq!(plan.confidence, Confidence::Partial);
        }
        let plan = cargo(&context(&f, "test", "cargo nextest run --profile ci")).unwrap();
        assert_eq!(plan.command, "cargo nextest run --profile ci");
        assert_eq!(plan.format, ReportFormat::JUnitXml);
        assert!(matches!(plan.files, ReportFiles::Globs { .. }));
        for command in ["cargo build", "cargo clippy", "cargo nextest list"] {
            assert!(cargo(&context(&f, "test", command)).is_none(), "{command}");
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
