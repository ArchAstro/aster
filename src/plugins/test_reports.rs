//! Capturing per-test results from each language's test runner.
//!
//! Each function recognises its runner in a plain command, the same way
//! [`super::related_tests`] does, and returns how to capture a report from
//! it. A command the function does not recognise gets no plan and runs as
//! written. Plans keep the runner's console output unchanged: a reporter is
//! added alongside the one the user sees, never in place of it. Pytest and
//! Jest still print a one-line notice naming the report file Aster added;
//! that line is dropped from the log.

use std::path::{Path, PathBuf};

use super::related_tests::{
    in_package_script, is_js_runner, is_script_call, js_runner, js_script, script_leaves,
    script_runs_no_runner, Argv, Delegated, Wrappers,
};
use super::LanguagePlugin;
use crate::config::{ReportConfig, ReportConfigFormat};
use crate::test_results::{exunit, nodetest};
use crate::test_results::{
    Confidence, OutputRewrite, ReportContext, ReportFiles, ReportFormat, ReportPlan, REPORT_DIR_ENV,
};

/// Plan for a target that declares its report in aster.toml. The command
/// runs as written, with [`REPORT_DIR_ENV`] naming the context's empty
/// report directory, and the declared files are read afterwards: the globs
/// under the project directory, or everything in the report directory.
pub(crate) fn declared(report: &ReportConfig, ctx: &ReportContext) -> ReportPlan {
    let (format, confidence) = match report.format {
        ReportConfigFormat::Junit => (ReportFormat::JUnitXml, Confidence::Complete),
        ReportConfigFormat::GoJson => (ReportFormat::GoTestJson, Confidence::Complete),
        ReportConfigFormat::JestJson => (ReportFormat::JestJson, Confidence::Complete),
        ReportConfigFormat::Libtest => (ReportFormat::LibtestText, Confidence::Partial),
    };
    let files = if report.files.is_empty() {
        ReportFiles::Globs {
            root: ctx.report_dir.to_path_buf(),
            patterns: vec!["**/*".to_string()],
        }
    } else {
        ReportFiles::Globs {
            root: ctx.project_dir.to_path_buf(),
            patterns: report.files.clone(),
        }
    };
    ReportPlan {
        command: ctx.command.to_string(),
        env: vec![(
            REPORT_DIR_ENV.to_string(),
            ctx.report_dir.to_string_lossy().into_owned(),
        )],
        files,
        format,
        confidence,
        rewrite: None,
        relative_to: Some(ctx.project_dir.to_path_buf()),
        declared: true,
    }
}

/// The report plan for one command of a target, the single place the
/// executor and `aster check` both ask.
///
/// A report the target declares wins: it says what the command really runs,
/// which recognising the command text can only guess. Otherwise the plugin
/// and then the language-independent recogniser read the command. A command
/// that starts with a configured `[affected.related] wrappers` program is
/// read as the command the wrapper runs, and the wrapper is written back in
/// front of the planned command, as narrowing does, so console output and
/// the wrapper's own behaviour are unchanged.
pub(crate) fn plan(
    plugin: &dyn LanguagePlugin,
    declared_report: Option<&ReportConfig>,
    ctx: &ReportContext,
    wrappers: &Wrappers,
) -> Option<ReportPlan> {
    if let Some(report) = declared_report {
        return Some(declared(report, ctx));
    }
    let direct = |ctx: &ReportContext| {
        plan_direct(plugin, ctx).or_else(|| plan_in_one_liner(plugin, ctx, wrappers))
    };
    if let Some(wrapped) = wrappers.strip(ctx.command) {
        let inner = ReportContext {
            command: &wrapped.command,
            ..*ctx
        };
        if let Some(mut found) = direct(&inner) {
            found.command = wrapped.render(&found.command);
            return Some(found);
        }
    }
    direct(ctx)
}

/// A wrapper inside a shell one-liner: `bash -c 'mise exec x -- cargo test'`.
fn plan_in_one_liner(
    plugin: &dyn LanguagePlugin,
    ctx: &ReportContext,
    wrappers: &Wrappers,
) -> Option<ReportPlan> {
    let delegated = Delegated::parse_allowing_expansions(ctx.command)?;
    let wrapped = wrappers.strip(&delegated.command)?;
    let working_dir = match &delegated.dir {
        Some(dir) => ctx.working_dir.join(dir),
        None => ctx.working_dir.to_path_buf(),
    };
    let inner = ReportContext {
        working_dir: &working_dir,
        command: &wrapped.command,
        ..*ctx
    };
    let mut found = plan_direct(plugin, &inner)?;
    found.command = delegated.render(&wrapped.render(&found.command));
    Some(found)
}

fn plan_direct(plugin: &dyn LanguagePlugin, ctx: &ReportContext) -> Option<ReportPlan> {
    plugin.test_report_plan(ctx).or_else(|| recognise(ctx))
}

/// How a package script chain is started: the wrapper in front of the
/// script call, and the launcher (`pnpm`, `npm`) that starts each step the
/// way the script would have.
#[derive(Debug, Clone)]
pub(crate) struct Launch {
    prefix: String,
    launcher: String,
}

impl Launch {
    /// A step of the chain as it is started.
    pub(crate) fn apply(&self, step: &str) -> String {
        format!("{}{}", self.prefix, in_package_script(step, &self.launcher))
    }
}

/// One command of an expanded package script chain.
#[derive(Debug, Clone)]
pub(crate) struct ChainStep {
    /// The step as the script wrote it.
    pub leaf: String,
    /// How to capture the results of a test runner step, and its own report
    /// directory. `None` for a step that is not a test runner (`tsc`,
    /// `eslint`, a generator): it runs as written.
    pub plan: Option<(ReportPlan, PathBuf)>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChainPlan {
    pub launch: Launch,
    pub steps: Vec<ChainStep>,
}

impl ChainPlan {
    pub(crate) fn remove_report_dirs(&self) {
        for (_, dir) in self.steps.iter().filter_map(|step| step.plan.as_ref()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// What reading a command as a package script chain found.
pub(crate) enum Chain {
    /// Not a chain with test runners in it, or one that cannot be read
    /// (`cd`, a `pre`/`post` hook, a cycle, another shell operator).
    Single,
    /// A chain with a step Aster cannot plan or read: a runner whose flags or
    /// config choose their own reporters, or a call to a script it could not
    /// expand.
    Unplanned {
        step: String,
    },
    Plan(ChainPlan),
}

/// A package script (`npm`/`pnpm`) whose body is a chain of commands
/// (`vitest run a && tsc --noEmit && vitest run b`) is expanded into the
/// commands `--related` expands it into. Each test runner step gets its own
/// report plan and report directory; other steps run as written. `new_dir`
/// makes a report directory per runner step.
pub(crate) fn chain(
    plugin: &dyn LanguagePlugin,
    ctx: &ReportContext,
    wrappers: &Wrappers,
    new_dir: &mut dyn FnMut() -> Option<PathBuf>,
) -> Chain {
    let (prefix, inner) = match wrappers.strip(ctx.command) {
        Some(wrapped) => (wrapped.prefix, wrapped.command),
        None => (String::new(), ctx.command.to_string()),
    };
    let Some((launcher, leaves)) = script_leaves(ctx.working_dir, &inner) else {
        return Chain::Single;
    };
    let holds_runner = |leaf: &String| {
        is_js_runner(leaf)
            || (is_script_call(leaf) && !script_runs_no_runner(ctx.working_dir, leaf))
    };
    if !leaves.iter().any(holds_runner) {
        return Chain::Single;
    }
    let mut steps: Vec<ChainStep> = Vec::new();
    let unplanned = |steps: &[ChainStep], step: String| {
        for (_, dir) in steps.iter().filter_map(|step| step.plan.as_ref()) {
            let _ = std::fs::remove_dir_all(dir);
        }
        Chain::Unplanned { step }
    };
    for leaf in leaves {
        if !is_js_runner(&leaf) {
            // A call that runs no runner (a build, a generator) runs as
            // written, hooks and all. One that holds a runner but was not
            // expanded (a script with a hook) is planned as the script call
            // it is.
            if !is_script_call(&leaf) || script_runs_no_runner(ctx.working_dir, &leaf) {
                steps.push(ChainStep { leaf, plan: None });
                continue;
            }
        }
        let Some(dir) = new_dir() else {
            return unplanned(&steps, leaf);
        };
        let step_ctx = ReportContext {
            command: &leaf,
            report_dir: &dir,
            ..*ctx
        };
        match plan_direct(plugin, &step_ctx) {
            Some(plan) => steps.push(ChainStep {
                leaf,
                plan: Some((plan, dir)),
            }),
            None => {
                let _ = std::fs::remove_dir_all(&dir);
                return unplanned(&steps, leaf);
            }
        }
    }
    Chain::Plan(ChainPlan {
        launch: Launch { prefix, launcher },
        steps,
    })
}

/// [`chain`] for a target: not when the target declares its report or one
/// complete plan already covers its command. The decision the executor and
/// `aster check` both ask, after [`plan`].
pub(crate) fn chain_for_target(
    plugin: &dyn LanguagePlugin,
    declared_report: Option<&ReportConfig>,
    ctx: &ReportContext,
    wrappers: &Wrappers,
    new_dir: &mut dyn FnMut() -> Option<PathBuf>,
) -> Chain {
    if declared_report.is_some() {
        return Chain::Single;
    }
    // A package script is read as the steps it consists of: each runner step
    // gets its own report, and a failed test is retried from the step's own
    // runner command, which the script's arguments could not narrow.
    match chain(plugin, ctx, wrappers, new_dir) {
        Chain::Unplanned { step } => {
            if plan(plugin, None, ctx, wrappers).is_some() {
                Chain::Single
            } else {
                Chain::Unplanned { step }
            }
        }
        found => found,
    }
}

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
        .or_else(|| node_test(ctx))
}

/// Node's built-in test runner, for a Node project's test command that no
/// other runner claims, such as a package script that chains several
/// `node --test` (or `tsx --test`) runs.
///
/// The command runs as written. `NODE_OPTIONS` adds Aster's reporter next
/// to `spec`, Node's console reporter since Node 23, so every `node --test`
/// the command starts appends its tests to one report, and other Node
/// processes ignore the flags. Aster cannot tell whether the command runs
/// tests some other way too, so the results are partial, unless the command is the `node --test` invocation itself (or a script that is only that), which is complete. Only a command
/// that mentions `--test`, or runs a package script that does, is planned,
/// and one that picks its own test reporter is left alone.
pub(super) fn node_test(ctx: &ReportContext) -> Option<ReportPlan> {
    let manifest = std::fs::read_to_string(ctx.working_dir.join("package.json")).ok()?;
    let mentions_test = |text: &str| {
        text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .any(|word| word == "--test")
    };
    let script = package_script(ctx.command, &manifest);
    let script = script.as_deref().unwrap_or("");
    if !mentions_test(ctx.command) && !mentions_test(script) {
        return None;
    }
    if script.contains("--test-reporter") {
        return None;
    }
    let existing = std::env::var("NODE_OPTIONS").unwrap_or_default();
    if [ctx.command, existing.as_str()]
        .iter()
        .any(|text| text.contains("--test-reporter"))
        || (ctx.command.contains("NODE_OPTIONS")
            && !ctx.command.contains("${NODE_OPTIONS")
            && !ctx.command.contains("$NODE_OPTIONS"))
    {
        return None;
    }
    // The command is the `node --test` invocation itself, or a package script
    // that is nothing else: Aster planned it from the argv. Only an opaque
    // command that mentions `--test` (a shell chain, a script file) may run
    // tests some other way too.
    let direct = is_js_runner(ctx.command) || is_js_runner(script);
    let reporter = ctx.report_dir.join("aster_node_test_reporter.mjs");
    std::fs::write(&reporter, nodetest::REPORTER_SOURCE).ok()?;
    let report = ctx.report_dir.join("node-test.ndjson");
    let quoted = reporter
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let added = format!(
        "--test-reporter=spec --test-reporter-destination=stdout \
         --test-reporter=\"{quoted}\" --test-reporter-destination=stderr"
    );
    let node_options = if existing.trim().is_empty() {
        added
    } else {
        format!("{existing} {added}")
    };
    Some(ReportPlan {
        command: ctx.command.to_string(),
        env: vec![
            ("NODE_OPTIONS".to_string(), node_options),
            (
                nodetest::REPORT_ENV.to_string(),
                report.display().to_string(),
            ),
        ],
        files: ReportFiles::Paths(vec![report]),
        format: ReportFormat::NodeTest,
        confidence: if direct {
            Confidence::Complete
        } else {
            Confidence::Partial
        },
        rewrite: None,
        // Node names test files by their resolved path.
        relative_to: Some(
            std::fs::canonicalize(ctx.working_dir)
                .unwrap_or_else(|_| ctx.working_dir.to_path_buf()),
        ),
        declared: false,
    })
}

/// The body of the package script a command runs: `pnpm test-ci`,
/// `npm run test-ci`, `yarn test`.
fn package_script(command: &str, manifest: &str) -> Option<String> {
    let argv = Argv::parse(command)?;
    let mut parts = argv.parts[argv.program..].iter().map(String::as_str);
    if !matches!(parts.next()?, "npm" | "pnpm" | "yarn" | "bun") {
        return None;
    }
    let name = match parts.next()? {
        "run" | "run-script" => parts.next()?,
        name => name,
    };
    let manifest: serde_json::Value = serde_json::from_str(manifest).ok()?;
    Some(manifest.get("scripts")?.get(name)?.as_str()?.to_string())
}

/// `cargo test` and `cargo nextest run`.
///
/// `cargo test` is read from its console output; the results are complete
/// when every binary's summary is there and adds up (see `libtest`).
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
        confidence: Confidence::Complete,
        rewrite: None,
        relative_to: None,
        declared: false,
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
pub(super) enum JsRunner {
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

    // `pnpm test` or `npm run test`, possibly with arguments for the
    // runner after it, as a retry's narrowed command has.
    let (script, passed) = js_script(&argv)
        .map(|script| (script.to_string(), Vec::new()))
        .or_else(|| js_script_with_args(&argv))?;
    let script = script.as_str();
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
    if launcher == "npm" && !passed.iter().any(|a| a == "--") {
        add.push("--".to_string());
    }
    add.extend(flags);
    Some(ReportPlan {
        command: argv.render_inserting(usize::MAX, &add),
        ..plan
    })
}

/// [`js_script`] for a script call followed by arguments:
/// `npm test -- a.test.ts -t name`, `pnpm run test a.test.ts`. Returns the
/// script name and the arguments.
fn js_script_with_args(argv: &Argv) -> Option<(String, Vec<String>)> {
    let parts = &argv.parts[argv.program..];
    let launcher = std::path::Path::new(parts.first()?).file_name()?.to_str()?;
    let (script, rest) = match parts.get(1)?.as_str() {
        "run" if matches!(launcher, "npm" | "pnpm" | "yarn" | "bun") => {
            (parts.get(2)?, &parts[3..])
        }
        "test" if matches!(launcher, "npm" | "pnpm" | "yarn") => (&parts[1], &parts[2..]),
        _ => return None,
    };
    // npm reads arguments before `--` as its own options.
    if launcher == "npm" && rest.first().is_some_and(|a| a != "--") {
        return None;
    }
    Some((script.clone(), rest.to_vec()))
}

pub(super) fn js_runner_kind(argv: &Argv, start: usize) -> Option<JsRunner> {
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
        declared: false,
    };
    match runner {
        JsRunner::Vitest => {
            let reporters = flag_values(argv, start, &["--reporter"]);
            let own_junit = reporters.iter().any(|(name, _)| name == "junit");
            if own_junit {
                // The command writes its own JUnit report. A second
                // `--outputFile.junit` makes Vitest hand its JUnit reporter
                // an array, which throws in `onInit` before any test runs, so
                // read the command's file instead of adding a reporter.
                if let Some(path) = own_junit_path(ctx.working_dir, argv, start, &reporters) {
                    let files = path.map_or_else(Vec::new, |path| vec![path]);
                    let confidence = if files.is_empty() {
                        Confidence::Partial
                    } else {
                        Confidence::Complete
                    };
                    return Some((
                        Vec::new(),
                        ReportPlan {
                            files: ReportFiles::Paths(files),
                            confidence,
                            ..plan(ctx.report_dir.join("vitest.xml"), ReportFormat::JUnitXml)
                        },
                    ));
                }
            }
            let file = ctx.report_dir.join("vitest.xml");
            let mut flags = Vec::new();
            if reporters.is_empty() {
                if vitest_config_sets_reporters(ctx.working_dir, argv, start) {
                    return None;
                }
                flags.push("--reporter=default".to_string());
                if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                    flags.push("--reporter=github-actions".to_string());
                }
            }
            if !own_junit {
                flags.push("--reporter=junit".to_string());
            }
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
            Some((
                flags,
                ReportPlan {
                    rewrite: Some(OutputRewrite::DropReportNotices),
                    ..plan(file, ReportFormat::JestJson)
                },
            ))
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

/// The values of a flag given as `--name=value` or `--name value`, with the
/// index of the argument that holds each value.
fn flag_values(argv: &Argv, start: usize, names: &[&str]) -> Vec<(String, usize)> {
    let parts = &argv.parts;
    let mut found = Vec::new();
    for (i, part) in parts.iter().enumerate().skip(start) {
        for name in names {
            if let Some(value) = part.strip_prefix(&format!("{name}=")) {
                found.push((value.to_string(), i));
            } else if part == name && parts.get(i + 1).is_some_and(|v| !v.starts_with('-')) {
                found.push((parts[i + 1].clone(), i + 1));
            }
        }
    }
    found
}

/// Reporters that never write to `--outputFile`, so a plain `--outputFile`
/// next to `--reporter=junit` can only be the JUnit report's.
const CONSOLE_REPORTERS: &[&str] = &[
    "default",
    "verbose",
    "dot",
    "tree",
    "agent",
    "github-actions",
    "hanging-process",
];

/// Where a command that already passes `--reporter=junit` has Vitest write
/// its report, as an absolute path. `None` when the command names no file
/// (the report goes to the console, or the config decides). `Some(None)`
/// when it names one that Aster cannot work out because it depends on a
/// shell expansion with no known value.
///
/// Vitest takes the file from `--outputFile.junit=<path>`, or from a plain
/// `--outputFile=<path>` that applies to every reporter that writes files, so
/// the plain form counts only when no other file reporter is listed. Paths
/// are relative to `--root`, which is relative to the working directory.
fn own_junit_path(
    dir: &Path,
    argv: &Argv,
    start: usize,
    reporters: &[(String, usize)],
) -> Option<Option<std::path::PathBuf>> {
    let specific = flag_values(argv, start, &["--outputFile.junit", "--output-file.junit"]);
    let (value, at) = match specific.last() {
        Some(found) => found.clone(),
        None => {
            let only_junit = reporters
                .iter()
                .all(|(name, _)| name == "junit" || CONSOLE_REPORTERS.contains(&name.as_str()));
            let plain = flag_values(argv, start, &["--outputFile", "--output-file"]);
            match plain.last() {
                Some(found) if only_junit => found.clone(),
                _ => return None,
            }
        }
    };
    let resolve = |value: &str, at: usize| -> Option<String> {
        if value.starts_with('~') {
            None
        } else if argv.expanding.get(at).is_some_and(Option::is_some) {
            expand_known(value)
        } else {
            Some(value.to_string())
        }
    };
    let root = match flag_values(argv, start, &["--root", "-r"]).last() {
        Some((root, at)) => resolve(root, *at).map(|root| dir.join(root)),
        None => Some(dir.to_path_buf()),
    };
    Some(
        resolve(&value, at)
            .zip(root)
            .map(|(path, root)| root.join(path)),
    )
}

/// Expand `$NAME`, `${NAME}`, `${NAME:-default}` and `${NAME-default}` in a
/// path from Aster's own environment, which the command inherits. `None` when
/// the text uses any other expansion (or `~`), or expands to nothing.
///
/// This is a guess about what the shell will see: a one-liner that sets the
/// variable itself before the command is not seen. The report is only read
/// if the command wrote it during the run, and a wrong guess ends in the
/// "captured 0 tests" warning rather than another run's results.
fn expand_known(text: &str) -> Option<String> {
    if text.starts_with('~') {
        return None;
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let (name, default, after) = if let Some(braced) = rest.strip_prefix('{') {
            let end = braced.find('}')?;
            let body = &braced[..end];
            let name_len = body
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(body.len());
            let (name, operator) = body.split_at(name_len);
            let default = match operator {
                "" => None,
                _ => Some((
                    operator.starts_with(":-"),
                    operator
                        .strip_prefix(":-")
                        .or_else(|| operator.strip_prefix('-'))?,
                )),
            };
            (name, default, &braced[end + 1..])
        } else {
            let name_len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (&rest[..name_len], None, &rest[name_len..])
        };
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let value = std::env::var(name).ok();
        match (value, default) {
            (Some(value), Some((true, default))) if value.is_empty() => {
                out.push_str(&expand_known(default)?)
            }
            (Some(value), _) => out.push_str(&value),
            (None, Some((_, default))) => out.push_str(&expand_known(default)?),
            (None, None) => {}
        }
        rest = after;
    }
    out.push_str(rest);
    (!out.is_empty()).then_some(out)
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
    let delegated = Delegated::parse_allowing_expansions(ctx.command)?;
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
    // The command is rebuilt from its parts below, which loses expansions.
    if argv.parts[argv.program] != "mix" || !argv.expanding.is_empty() {
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
            declared: false,
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
        expanding: Vec::new(),
    };
    Some(ReportPlan {
        command: rewritten.render_inserting(usize::MAX, &[]),
        env: vec![(exunit::REPORT_ENV.to_string(), report.display().to_string())],
        files: ReportFiles::Paths(vec![report]),
        format: ReportFormat::ExUnit,
        confidence: Confidence::Complete,
        rewrite: None,
        relative_to: None,
        declared: false,
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
    // `uv run --with pytest -- python -m pytest`: the first `pytest` is the
    // value of a wrapper option, not the runner. Skip those so the report
    // flag lands after the runner, inside the wrapped command.
    let runner = argv.parts.iter().enumerate().position(|(i, p)| {
        let name = Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        let is_option_value = i > 0
            && matches!(
                argv.parts[i - 1].as_str(),
                "--with"
                    | "-w"
                    | "--with-editable"
                    | "--from"
                    | "--package"
                    | "--with-requirements"
            );
        (name == "pytest" || name == "py.test") && !is_option_value
    })?;
    let plan = |command: String, file: std::path::PathBuf, rewrite| ReportPlan {
        command,
        env: Vec::new(),
        files: ReportFiles::Paths(vec![file]),
        format: ReportFormat::JUnitXml,
        confidence: Confidence::Complete,
        rewrite,
        relative_to: None,
        declared: false,
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
            // The user already chose the report file. Its notice does not
            // name Aster's directory, so the log is left as written.
            return Some(plan(
                ctx.command.to_string(),
                ctx.working_dir.join(path),
                None,
            ));
        }
        i += 1;
    }

    let file = ctx.report_dir.join("pytest.xml");
    let command = argv.render_inserting(end, &[format!("--junitxml={}", file.display())]);
    Some(plan(command, file, Some(OutputRewrite::DropReportNotices)))
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
        declared: false,
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
        declared: false,
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
        declared: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_results::TestOutcome;
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

    fn node_project(manifest: &str) -> (tempfile::TempDir, Fixture) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), manifest).unwrap();
        let f = Fixture {
            project: tmp.path().to_path_buf(),
            report_dir: tmp.path().join("reports dir"),
        };
        std::fs::create_dir_all(&f.report_dir).unwrap();
        (tmp, f)
    }

    #[test]
    fn node_test_is_complete_when_aster_reads_the_invocation_itself() {
        let (_tmp, f) = node_project(
            r#"{"scripts": {"test": "node --test a.test.js", "chain": "pnpm lint && node --test"}}"#,
        );
        for command in ["node --test a.test.js", "tsx --test", "pnpm run test"] {
            let plan = node_test(&context(&f, "test", command)).unwrap();
            assert_eq!(plan.confidence, Confidence::Complete, "{command}");
        }
        // An opaque command that merely mentions `--test` may run tests
        // another way too.
        for command in ["pnpm run chain", "bash -c 'x && node --test'"] {
            let plan = node_test(&context(&f, "test", command)).unwrap();
            assert_eq!(plan.confidence, Confidence::Partial, "{command}");
        }
    }

    #[test]
    fn node_test_adds_aster_reporter_through_node_options() {
        let (_tmp, f) = node_project(
            r#"{"scripts": {"test-ci": "pnpm lint && tsx --test --test-force-exit tests/a.test.ts"}}"#,
        );
        let plan = node_test(&context(&f, "test-ci", "pnpm test-ci")).unwrap();
        assert_eq!(plan.command, "pnpm test-ci");
        assert_eq!(plan.format, ReportFormat::NodeTest);
        assert_eq!(plan.confidence, Confidence::Partial);
        let reporter = f.report_dir.join("aster_node_test_reporter.mjs");
        assert_eq!(
            std::fs::read_to_string(&reporter).unwrap(),
            nodetest::REPORTER_SOURCE
        );
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        let options = &env["NODE_OPTIONS"];
        assert!(
            options.ends_with(&format!(
                "--test-reporter=spec --test-reporter-destination=stdout \
                 --test-reporter=\"{}\" --test-reporter-destination=stderr",
                reporter.display()
            )),
            "{options}"
        );
        let report = f.report_dir.join("node-test.ndjson");
        assert_eq!(env[nodetest::REPORT_ENV], report.display().to_string());
        assert_eq!(plan.files, ReportFiles::Paths(vec![report]));
    }

    #[test]
    fn node_test_leaves_commands_without_node_tests_or_with_their_own_reporter() {
        let (_tmp, f) = node_project(
            r#"{"scripts": {"test": "eslint . && tsc --noEmit", "unit": "node --test"}}"#,
        );
        assert_eq!(node_test(&context(&f, "test", "pnpm test")), None);
        assert_eq!(node_test(&context(&f, "deps", "pnpm install")), None);
        assert!(node_test(&context(&f, "unit", "npm run unit")).is_some());
        assert!(node_test(&context(&f, "test", "node --test")).is_some());
        assert_eq!(
            node_test(&context(&f, "test", "node --test --test-reporter=tap")),
            None
        );
        assert_eq!(
            node_test(&context(&f, "test", "NODE_OPTIONS=--inspect node --test")),
            None
        );
        let (_tmp, f) =
            node_project(r#"{"scripts": {"test": "node --test --test-reporter=junit"}}"#);
        assert_eq!(node_test(&context(&f, "test", "pnpm test")), None);
        let none = Fixture {
            project: PathBuf::from("/nonexistent"),
            report_dir: PathBuf::from("/nonexistent/r"),
        };
        assert_eq!(node_test(&context(&none, "test", "node --test")), None);
    }

    /// Runs real `node --test` processes with the planned environment, as
    /// a package script chaining two of them would, and reads the report.
    #[test]
    fn node_test_report_records_every_test_of_every_run() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: node is not installed");
            return;
        }
        let (_tmp, f) = node_project(r#"{"scripts": {"test": "node --test"}}"#);
        std::fs::write(
            f.project.join("a.test.mjs"),
            r#"import test, { describe, it } from "node:test";
import assert from "node:assert";
describe("outer", () => {
  it("passes", () => {});
  it("fails", () => assert.equal(1, 2));
  it.skip("skipped", () => {});
});
test("parent", async (t) => { await t.test("child", () => {}); });
"#,
        )
        .unwrap();
        std::fs::write(
            f.project.join("b.test.mjs"),
            "throw new Error(\"broken\");\n",
        )
        .unwrap();
        let plan = node_test(&context(&f, "test", "pnpm test")).unwrap();
        let started = std::time::SystemTime::now();
        for file in ["a.test.mjs", "b.test.mjs"] {
            let output = std::process::Command::new("node")
                .args(["--test", file])
                .current_dir(&f.project)
                .envs(plan.env.iter().cloned())
                .output()
                .unwrap();
            assert!(!output.status.success());
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("tests "), "spec still prints: {stdout}");
        }
        let collected = crate::test_results::collect(&plan, started, None).unwrap();
        let mut seen: Vec<_> = collected
            .cases
            .iter()
            .map(|c| (c.file.clone().unwrap(), c.name.clone(), c.outcome))
            .collect();
        seen.sort_by(|x, y| (&x.0, &x.1).cmp(&(&y.0, &y.1)));
        let a = |name: &str, outcome| ("a.test.mjs".to_string(), name.to_string(), outcome);
        assert_eq!(
            seen,
            vec![
                a("outer > fails", TestOutcome::Fail),
                a("outer > passes", TestOutcome::Pass),
                a("outer > skipped", TestOutcome::Skip),
                a("parent > child", TestOutcome::Pass),
                (
                    "b.test.mjs".to_string(),
                    "b.test.mjs".to_string(),
                    TestOutcome::Error
                ),
            ]
        );
        let failed = collected
            .cases
            .iter()
            .find(|c| c.name == "outer > fails")
            .unwrap();
        assert!(failed.message.as_deref().unwrap().contains("1 == 2"));
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

    fn vitest_paths(plan: &ReportPlan) -> Vec<PathBuf> {
        match &plan.files {
            ReportFiles::Paths(paths) => paths.clone(),
            other => panic!("expected paths, got {other:?}"),
        }
    }

    #[test]
    fn vitest_with_its_own_junit_report_reads_that_file_and_adds_nothing() {
        let (_tmp, f) = js_fixture("{}", &[]);
        // Both forms of the per-reporter option, and the plain option when
        // junit is the only file reporter.
        for command in [
            "vitest run e2e --reporter=default --reporter=junit --outputFile.junit=out/r.xml",
            "vitest run e2e --reporter default --reporter junit --outputFile.junit out/r.xml",
            "vitest run e2e --reporter=junit --outputFile=out/r.xml",
            "vitest run e2e --reporter=junit --output-file.junit=out/r.xml",
        ] {
            let plan = js(&context(&f, "test", command)).unwrap();
            // A second `--outputFile.junit` makes Vitest throw before any
            // test runs, so the command is not touched.
            assert_eq!(plan.command, command);
            assert_eq!(
                vitest_paths(&plan),
                [f.project.join("out/r.xml")],
                "{command}"
            );
            assert_eq!(plan.confidence, Confidence::Complete);
        }

        // A relative path is relative to `--root`, and an absolute one stays.
        let plan = js(&context(
            &f,
            "test",
            "vitest run --root web --reporter=junit --outputFile.junit=r.xml",
        ))
        .unwrap();
        assert_eq!(vitest_paths(&plan), [f.project.join("web/r.xml")]);
        let plan = js(&context(
            &f,
            "test",
            "vitest run --reporter=junit --outputFile.junit=/tmp/r.xml",
        ))
        .unwrap();
        assert_eq!(vitest_paths(&plan), [PathBuf::from("/tmp/r.xml")]);
    }

    #[test]
    fn vitest_plain_output_file_is_not_junits_when_another_file_reporter_is_listed() {
        let (_tmp, f) = js_fixture("{}", &[]);
        // `--outputFile` also applies to the json reporter, so the report
        // may not be JUnit: fall back to adding Aster's own, which names
        // its file per reporter.
        let plan = js(&context(
            &f,
            "test",
            "vitest run --reporter=json --reporter=junit --outputFile=out.json",
        ))
        .unwrap();
        assert_eq!(
            vitest_paths(&plan),
            [PathBuf::from("/tmp/aster-reports/1/vitest.xml")]
        );
        assert!(plan
            .command
            .contains("--outputFile.junit=/tmp/aster-reports/1/vitest.xml"));
        assert_eq!(plan.command.matches("--reporter=junit").count(), 1);
    }

    #[test]
    fn vitest_junit_to_the_console_gets_a_file_without_a_second_reporter() {
        let (_tmp, f) = js_fixture("{}", &[]);
        let plan = js(&context(&f, "test", "vitest run --reporter=junit")).unwrap();
        assert_eq!(
            plan.command,
            "vitest run --outputFile.junit=/tmp/aster-reports/1/vitest.xml --reporter=junit"
        );
    }

    #[test]
    fn vitest_junit_path_from_the_environment_is_expanded_or_left_unread() {
        let (_tmp, f) = js_fixture("{}", &[]);
        // The real firstlanding e2e command: an unset variable takes its
        // default, and the expansion stays in the command as written.
        let command = "bash -c 'pnpm exec vitest run --config v.ts ${ASTER_TEST_UNSET_FILE:-e2e} \
             --reporter=default --reporter=junit \
             --outputFile.junit=\"${ASTER_TEST_UNSET_OUT:-/tmp/web-e2e.xml}\"'";
        let plan = recognise(&context(&f, "test", command)).unwrap();
        let script = shell_words::split(&plan.command).unwrap().pop().unwrap();
        assert!(script.contains("${ASTER_TEST_UNSET_FILE:-e2e}"), "{script}");
        assert!(
            script.contains("--outputFile.junit=\"${ASTER_TEST_UNSET_OUT:-/tmp/web-e2e.xml}\""),
            "{script}"
        );
        assert_eq!(script.matches("--outputFile.junit").count(), 1);
        assert_eq!(vitest_paths(&plan), [PathBuf::from("/tmp/web-e2e.xml")]);

        // A variable the one-liner sets itself, or any other expansion,
        // cannot be known: nothing is added (a duplicate would crash
        // Vitest) and the results are partial.
        for path in ["$ASTER_TEST_UNSET_DIR", "~/r.xml", "${X:=a}"] {
            let command =
                format!("bash -c 'vitest run --reporter=junit --outputFile.junit={path}'");
            let plan = recognise(&context(&f, "test", &command)).unwrap();
            assert!(vitest_paths(&plan).is_empty(), "{path}");
            assert_eq!(plan.confidence, Confidence::Partial);
            assert_eq!(plan.command.matches("--outputFile").count(), 1);
        }
    }

    #[test]
    fn expand_known_reads_the_environment_and_refuses_the_rest() {
        std::env::set_var("ASTER_TEST_SET", "set");
        std::env::set_var("ASTER_TEST_EMPTY", "");
        for (text, expected) in [
            ("a/$ASTER_TEST_SET/b", Some("a/set/b")),
            ("${ASTER_TEST_SET}x", Some("setx")),
            ("${ASTER_TEST_SET:-d}", Some("set")),
            ("${ASTER_TEST_NOPE:-d/e}", Some("d/e")),
            ("${ASTER_TEST_NOPE-d}", Some("d")),
            ("${ASTER_TEST_EMPTY:-d}", Some("d")),
            ("${ASTER_TEST_EMPTY-d}", Some("")),
            ("$ASTER_TEST_NOPE", None),
            ("${ASTER_TEST_NOPE:-$ASTER_TEST_SET}", Some("set")),
            ("${ASTER_TEST_NOPE:=d}", None),
            ("${#ASTER_TEST_SET}", None),
            ("$1", None),
            ("~/x", None),
            ("plain", Some("plain")),
        ] {
            assert_eq!(
                expand_known(text).as_deref(),
                expected.filter(|e| !e.is_empty()),
                "{text}"
            );
        }
    }

    #[test]
    fn delegating_treats_shell_expansions_as_opaque_arguments() {
        let (_tmp, f) = js_fixture("{}", &[]);
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        let github = if ci { " --reporter=github-actions" } else { "" };
        for command in [
            "bash -c 'vitest run --root . $QF'",
            "sh -c 'vitest run --root . ${QF:-flaky}'",
            "bash -c 'vitest run \"$QF\" -t \"a $B\"'",
            "bash -c 'FOO=$BAR exec vitest run'",
        ] {
            let plan = recognise(&context(&f, "test", command)).unwrap();
            let script = shell_words::split(&plan.command).unwrap().pop().unwrap();
            let expected = command.split_once('\'').unwrap().1.trim_end_matches('\'');
            // Only Aster's flags are new; every expansion is written back as
            // it was, not quoted into a literal.
            let stripped = script
                .replace(&format!("--reporter=default{github} "), "")
                .replace(
                    " --reporter=junit --outputFile.junit=/tmp/aster-reports/1/vitest.xml",
                    "",
                )
                .replace(
                    "--reporter=junit --outputFile.junit=/tmp/aster-reports/1/vitest.xml ",
                    "",
                );
            assert_eq!(
                stripped.split_whitespace().count(),
                expected.split_whitespace().count(),
                "{script}"
            );
            for word in expected.split_whitespace().filter(|w| w.contains('$')) {
                assert!(script.contains(word), "{word} in {script}");
            }
        }

        // The boundary: an expansion that decides what runs, or whose
        // value reshapes the arguments, is not planned.
        for command in [
            "bash -c '$RUNNER run'",
            "bash -c 'exec $RUNNER run'",
            "bash -c 'vitest run $(cat files)'",
            "bash -c 'vitest run `cat files`'",
            "bash -c 'vitest run ${QF:-a'",
        ] {
            assert!(
                recognise(&context(&f, "test", command)).is_none(),
                "{command}"
            );
        }
        // A literal `$` in single quotes is not an expansion.
        let plan = recognise(&context(&f, "test", "bash -c \"vitest run -t 'x$'\"")).unwrap();
        assert!(plan.command.contains("x$"));
    }

    /// Every runner behind a configured wrapper is planned as the command the
    /// wrapper runs, with the wrapper written back in front.
    #[test]
    fn plan_reads_a_runner_through_configured_wrappers() {
        use crate::config::{RelatedWrapperConfig, RelatedWrapperTable};
        use crate::plugins::PluginRegistry;
        let wrappers = Wrappers::new(&[
            RelatedWrapperConfig::Program("with-rust-toolchain".to_string()),
            RelatedWrapperConfig::Detailed(RelatedWrapperTable {
                program: "mise".to_string(),
                until: Some("--".to_string()),
            }),
        ]);
        let registry = PluginRegistry::with_all_plugins();
        let (_tmp, f) = node_project("{}");
        let plan_of = |plugin: &str, command: &str, wrappers: &Wrappers| {
            let plugin = registry.find_by_name(plugin).unwrap();
            plan(plugin, None, &context(&f, "test", command), wrappers)
        };
        let none = Wrappers::default();

        // The incident: `//evals/fix:test-ci`.
        let command = "mise exec go -- bun test --timeout=60000";
        assert!(plan_of("nodejs", command, &none).is_none());
        let bun = plan_of("nodejs", command, &wrappers).unwrap();
        assert!(
            bun.command
                .starts_with("mise exec go -- bun test --reporter=junit ")
                && bun.command.contains("bun.xml")
                && bun.command.ends_with(" --timeout=60000"),
            "{}",
            bun.command
        );
        assert_eq!(bun.format, ReportFormat::JUnitXml);
        assert_eq!(bun.confidence, Confidence::Complete);

        let cargo = plan_of("rust", "with-rust-toolchain cargo test -p x", &wrappers).unwrap();
        assert_eq!(cargo.command, "with-rust-toolchain cargo test -p x");
        assert_eq!(cargo.format, ReportFormat::LibtestText);

        let go = plan_of("go", "mise exec go@1.24 -- go test ./...", &wrappers).unwrap();
        assert!(
            go.command.starts_with("mise exec go@1.24 -- go test "),
            "{}",
            go.command
        );
        assert_eq!(go.format, ReportFormat::GoTestJson);

        let mix = plan_of("elixir", "mise exec elixir -- mix test", &wrappers).unwrap();
        assert!(
            mix.command.starts_with("mise exec elixir -- "),
            "{}",
            mix.command
        );
        assert!(mix.command.contains("mix test"), "{}", mix.command);

        let pytest = plan_of("python", "mise exec python -- pytest tests", &wrappers).unwrap();
        assert!(
            pytest.command.starts_with("mise exec python -- pytest "),
            "{}",
            pytest.command
        );
        assert_eq!(pytest.format, ReportFormat::JUnitXml);

        // Nested wrappers and assignments before the program keep their order.
        let nested = plan_of(
            "nodejs",
            "CI=1 with-rust-toolchain mise exec go -- bun test",
            &wrappers,
        )
        .unwrap();
        assert!(
            nested
                .command
                .starts_with("CI=1 with-rust-toolchain mise exec go -- bun test "),
            "{}",
            nested.command
        );

        // A wrapper inside a one-liner behind another wrapper.
        let one_liner = plan_of(
            "rust",
            "with-rust-toolchain bash -c 'A=1 mise exec x@1 -- cargo test --locked -p a'",
            &wrappers,
        )
        .unwrap();
        assert_eq!(
            one_liner.command,
            "with-rust-toolchain bash -c 'A=1 mise exec x@1 -- cargo test --locked -p a'"
        );
        assert_eq!(one_liner.format, ReportFormat::LibtestText);

        let real = plan_of(
            "rust",
            r#"../../../scripts/with-rust-toolchain bash -c 'ARCHDEV_SCCACHE_LIFECYCLE_REPO="$PWD/../../.." ARCHDEV_SCCACHE_LIFECYCLE_TMP=/tmp mise exec aqua:mozilla/sccache@0.18.0 -- cargo test --locked -p archdev --test sccache_scratch_lifecycle -- sccache_survives_prior_job_scratch_cleanup --ignored --exact --nocapture'"#,
            &wrappers,
        );
        assert!(real.is_some());
        let plain = plan_of("rust", r#"bash -c 'A="$PWD/.." cargo test'"#, &wrappers);
        assert!(plain.is_some(), "expansion in an assignment");

        // A program that is not configured still hides the runner.
        assert!(plan_of("nodejs", "unlisted exec -- bun test", &wrappers).is_none());

        // A declared report wins and runs the command as written.
        let declared_report = ReportConfig {
            format: ReportConfigFormat::Junit,
            files: Vec::new(),
        };
        let plugin = registry.find_by_name("nodejs").unwrap();
        let declared_plan = plan(
            plugin,
            Some(&declared_report),
            &context(&f, "test", command),
            &wrappers,
        )
        .unwrap();
        assert!(declared_plan.declared);
        assert_eq!(declared_plan.command, command);
    }

    #[test]
    fn a_package_script_chain_gets_a_report_plan_per_runner_step() {
        use crate::config::{RelatedWrapperConfig, RelatedWrapperTable};
        use crate::plugins::PluginRegistry;
        let manifest = r#"{"scripts":{
            "test-ci":"pnpm test:a && tsc --noEmit && vitest run b.test.ts",
            "test:a":"vitest run a.test.ts",
            "hooked":"vitest run a && tsc","prehooked":"true",
            "moved":"cd sub && vitest run && tsc",
            "own":"vitest run && tsc",
            "calls":"vitest run && pnpm build","build":"tsc && pnpm calls",
            "lint":"eslint . && tsc",
            "viahook":"pnpm run gen && vitest run","gen":"tsc -p x","pregen":"pnpm run prep","prep":"tsc",
            "hiddenhook":"pnpm run gen2 && vitest run","gen2":"tsc","pregen2":"vitest run --root y",
            "solo":"node --test a.test.js","hookrun":"pnpm run gen && pnpm run e2e","e2e":"vitest run --config e.ts","pree2e":"tsc","solo2":"pnpm solo"}}"#;
        let (_tmp, f) = node_project(manifest);
        let registry = PluginRegistry::with_all_plugins();
        let plugin = registry.find_by_name("nodejs").unwrap();
        let mise = Wrappers::new(&[RelatedWrapperConfig::Detailed(RelatedWrapperTable {
            program: "mise".to_string(),
            until: Some("--".to_string()),
        })]);
        let run = |command: &str, wrappers: &Wrappers| {
            chain_for_target(
                plugin,
                None,
                &context(&f, "test", command),
                wrappers,
                &mut || {
                    static NEXT: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let dir = f.report_dir.join(
                        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            .to_string(),
                    );
                    std::fs::create_dir_all(&dir).ok()?;
                    Some(dir)
                },
            )
        };

        // Two runner steps with their own report directories; tsc runs as
        // written; the launcher and wrapper start every step.
        let Chain::Plan(plan) = run("mise exec node -- pnpm test-ci", &mise) else {
            panic!("a chain");
        };
        let leaves: Vec<&str> = plan.steps.iter().map(|s| s.leaf.as_str()).collect();
        assert_eq!(
            leaves,
            [
                "vitest run a.test.ts",
                "tsc --noEmit",
                "vitest run b.test.ts"
            ]
        );
        let dirs: Vec<_> = plan
            .steps
            .iter()
            .filter_map(|s| s.plan.as_ref().map(|(_, dir)| dir.clone()))
            .collect();
        assert_eq!(dirs.len(), 2);
        assert_ne!(dirs[0], dirs[1]);
        assert!(plan.steps[1].plan.is_none());
        let planned = &plan.steps[2].plan.as_ref().unwrap().0;
        assert_eq!(planned.format, ReportFormat::JUnitXml);
        assert_eq!(planned.confidence, Confidence::Complete);
        assert!(
            planned.command.starts_with("vitest run "),
            "{}",
            planned.command
        );
        assert_eq!(
            plan.launch.apply("tsc --noEmit"),
            "mise exec node -- pnpm exec tsc --noEmit"
        );
        // The npm launcher quotes the whole step.
        let Chain::Plan(npm) = run("npm run test-ci", &Wrappers::default()) else {
            panic!("a chain");
        };
        assert_eq!(
            npm.launch.apply("tsc --noEmit"),
            "npm exec -c 'tsc --noEmit'"
        );

        // A call that has a hook is not expanded but runs as written when no
        // runner hides in it; one that hides a runner is not captured.
        let Chain::Plan(hooked) = run("pnpm viahook", &Wrappers::default()) else {
            panic!("a chain with a hooked call");
        };
        let leaves: Vec<&str> = hooked.steps.iter().map(|s| s.leaf.as_str()).collect();
        assert_eq!(leaves, ["pnpm run gen", "vitest run"]);
        assert!(hooked.steps[0].plan.is_none());
        hooked.remove_report_dirs();
        let e2e = match run("pnpm hookrun", &Wrappers::default()) {
            Chain::Plan(plan) => plan,
            Chain::Unplanned { step } => panic!("unplanned: {step}"),
            Chain::Single => panic!("single"),
        };
        let (planned, _) = e2e.steps[1].plan.as_ref().unwrap();
        assert!(
            planned.command.starts_with("pnpm run e2e --reporter="),
            "{}",
            planned.command
        );
        e2e.remove_report_dirs();
        assert!(matches!(
            run("pnpm hiddenhook", &Wrappers::default()),
            Chain::Unplanned { .. }
        ));
        // A script that is one `node --test` is a chain of one step, however
        // it is reached.
        let Chain::Plan(direct) = run("pnpm solo", &Wrappers::default()) else {
            panic!("one runner step");
        };
        direct.remove_report_dirs();
        let Chain::Plan(solo) = run("pnpm solo2", &Wrappers::default()) else {
            panic!("one runner step");
        };
        assert_eq!(solo.steps.len(), 1);
        solo.remove_report_dirs();

        // Chains that cannot be read stay uncaptured, as narrowing leaves them.
        for command in [
            "pnpm hooked",
            "pnpm moved",
            "pnpm lint",
            "pnpm test-ci --if-present",
        ] {
            assert!(
                matches!(run(command, &Wrappers::default()), Chain::Single),
                "{command}"
            );
        }
        // A runner step that picks its own reporters, or a call that is not
        // expanded, is named.
        match run("pnpm calls", &Wrappers::default()) {
            Chain::Unplanned { step } => assert!(step.starts_with("pnpm "), "{step}"),
            _ => panic!("a call that is not expanded"),
        }
        std::fs::write(
            f.project.join("vitest.config.ts"),
            "export default { test: { reporters: ['dot'] } }",
        )
        .unwrap();
        match run("pnpm own", &Wrappers::default()) {
            Chain::Unplanned { step } => assert_eq!(step, "vitest run"),
            _ => panic!("a runner whose config sets reporters"),
        }
        // One plan already covers a plain script; a declared report wins.
        std::fs::remove_file(f.project.join("vitest.config.ts")).unwrap();
        let Chain::Plan(plain) = run("pnpm run test:a", &Wrappers::default()) else {
            panic!("a plain script is a chain of one step");
        };
        plain.remove_report_dirs();
        let report = ReportConfig {
            format: ReportConfigFormat::Junit,
            files: Vec::new(),
        };
        let declared = chain_for_target(
            plugin,
            Some(&report),
            &context(&f, "test", "pnpm test-ci"),
            &Wrappers::default(),
            &mut || None,
        );
        assert!(matches!(declared, Chain::Single));
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

    #[test]
    fn a_balanced_partition_is_captured_like_the_partition_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let shard = tmp.path().join("platform-test-shard-2");
        let platform = tmp.path().join("platform");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::create_dir_all(platform.join("test")).unwrap();
        std::fs::write(platform.join("test/test_helper.exs"), "ExUnit.start()\n").unwrap();
        let report_dir = tmp.path().join("reports");
        std::fs::create_dir_all(&report_dir).unwrap();
        let original =
            "bash -c 'cd ../platform && MIX_TEST_PARTITION=2 exec mix test --partitions 4'";
        let command = crate::plugins::mix_partitions::parse(&shard, original)
            .unwrap()
            .render(&["test/a_test.exs".into(), "test/b_test.exs".into()]);
        let ctx = ReportContext {
            project_dir: &shard,
            working_dir: &shard,
            target_name: "test-ci",
            command: &command,
            report_dir: &report_dir,
        };
        let plan = delegating(&ctx, mix).unwrap();
        let script = shell_words::split(&plan.command).unwrap().pop().unwrap();
        assert!(
            script.starts_with("cd ../platform && MIX_TEST_PARTITION=2 exec elixir -r "),
            "{script}"
        );
        assert!(
            script.ends_with(
                "-S mix test --formatter AsterTestResults.ExUnitFormatter --formatter ExUnit.CLIFormatter test/a_test.exs test/b_test.exs"
            ),
            "{script}"
        );
        assert_eq!(plan.confidence, Confidence::Complete);
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
        // A retry's narrowed script call keeps its arguments, and npm's `--`
        // is not repeated.
        let plan = js(&context(&f, "test", "npm test -- a.test.ts -t 'x$'")).unwrap();
        assert!(
            plan.command
                .starts_with("npm test -- a.test.ts -t 'x$' --reporter=default"),
            "{}",
            plan.command
        );
        let plan = js(&context(&f, "test", "pnpm run test-ci a.test.ts")).unwrap();
        assert_eq!(
            plan.command,
            "pnpm run test-ci a.test.ts --json --outputFile=/tmp/aster-reports/1/jest.json"
        );
        assert_eq!(js(&context(&f, "test", "npm test a.test.ts")), None);
        let plan = js(&context(&f, "test", "pnpm run test-ci")).unwrap();
        assert_eq!(
            plan.command,
            "pnpm run test-ci --json --outputFile=/tmp/aster-reports/1/jest.json"
        );
        assert_eq!(plan.format, ReportFormat::JestJson);
        assert_eq!(plan.rewrite, Some(OutputRewrite::DropReportNotices));
        let plan = js(&context(&f, "test", "yarn run bun-test")).unwrap();
        assert_eq!(
            plan.command,
            "yarn run bun-test --reporter=junit --reporter-outfile=/tmp/aster-reports/1/bun.xml"
        );
        assert_eq!(plan.rewrite, None);
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
        // A direct Jest command gets the same notice filter as a script.
        let plan = js(&context(&f, "test", "jest --ci")).unwrap();
        assert_eq!(plan.rewrite, Some(OutputRewrite::DropReportNotices));
        assert!(matches!(plan.files, ReportFiles::Paths(_)));
        assert_ne!(plan.files, ReportFiles::Stdout);
    }

    #[test]
    fn a_declared_report_runs_the_command_as_written_and_names_the_report_dir() {
        let f = fixture();
        let ctx = context(&f, "test", "bash tools/cargo-test.sh");
        let into_dir = ReportConfig {
            format: ReportConfigFormat::Junit,
            files: Vec::new(),
        };
        let plan = declared(&into_dir, &ctx);
        assert_eq!(plan.command, "bash tools/cargo-test.sh");
        assert_eq!(
            plan.env,
            vec![(
                REPORT_DIR_ENV.to_string(),
                "/tmp/aster-reports/1".to_string()
            )]
        );
        assert_eq!(
            plan.files,
            ReportFiles::Globs {
                root: f.report_dir.clone(),
                patterns: vec!["**/*".to_string()],
            }
        );
        assert_eq!(
            (plan.format, plan.confidence),
            (ReportFormat::JUnitXml, Confidence::Complete)
        );

        let in_project = ReportConfig {
            format: ReportConfigFormat::Libtest,
            files: vec!["target/test-output.txt".to_string()],
        };
        let plan = declared(&in_project, &ctx);
        assert_eq!(
            plan.files,
            ReportFiles::Globs {
                root: f.project.clone(),
                patterns: vec!["target/test-output.txt".to_string()],
            }
        );
        // Console output can miss tests.
        assert_eq!(
            (plan.format, plan.confidence),
            (ReportFormat::LibtestText, Confidence::Partial)
        );
        for (format, want) in [
            (ReportConfigFormat::GoJson, ReportFormat::GoTestJson),
            (ReportConfigFormat::JestJson, ReportFormat::JestJson),
        ] {
            let plan = declared(
                &ReportConfig {
                    format,
                    files: Vec::new(),
                },
                &ctx,
            );
            assert_eq!((plan.format, plan.confidence), (want, Confidence::Complete));
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
            assert_eq!(plan.confidence, Confidence::Complete);
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
    fn js_never_adds_report_flags_to_a_wrapper() {
        let f = fixture();
        let plan = js(&context(&f, "test", "mise exec node@24 -- vitest run"));
        assert!(plan.is_none());
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
                "uv run --quiet --no-project --with pytest -- python -m pytest -q test_main.py",
                "uv run --quiet --no-project --with pytest -- python -m pytest -q test_main.py --junitxml=/tmp/aster-reports/1/pytest.xml",
            ),
            (
                "uvx --from pytest pytest -q",
                "uvx --from pytest pytest -q --junitxml=/tmp/aster-reports/1/pytest.xml",
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
            assert_eq!(plan.rewrite, Some(OutputRewrite::DropReportNotices));
            assert_ne!(plan.files, ReportFiles::Stdout);
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
            assert_eq!(plan.rewrite, None);
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
