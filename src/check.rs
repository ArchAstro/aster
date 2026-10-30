//! `aster check`: a lint of the configuration and annotations that
//! `aster affected --related` depends on.
//!
//! Every rule has a stable id. Findings are warnings or errors; the command
//! changes no selection. See the README, "Checking the configuration".

use crate::cli::affected_files::{
    called_scripts, reachable_targets, suite_runs, test_targets, uncaptured_reason, TestTarget,
};
use crate::config::WorkspaceConfig;
use crate::discovery::DiscoveredProject;
use crate::git::select_affected_lane;
use crate::plugins::related_tests::Wrappers;
use crate::plugins::{test_reports, PluginRegistry};
use crate::related::{self, SourceFinding, Sources};
use crate::test_results::{new_report_dir, ReportContext};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const UNNARROWABLE: &str = "unnarrowable-test-command";
pub const SUSPICIOUS: &str = "suspicious-path-literal";
pub const BAD_CONSUMES: &str = "bad-consumes";
pub const ORPHAN: &str = "orphan-test-target";
pub const STALE: &str = "stale-annotation";
pub const STALE_ALLOW: &str = "stale-allow-orphan";
pub const STALE_ALLOW_UNNARROWABLE: &str = "stale-allow-unnarrowable";
pub const UNCAPTURED: &str = "uncaptured-test-results";
pub const STALE_ALLOW_NO_RESULTS: &str = "stale-allow-no-test-results";

/// Every rule id, in the order findings are listed.
pub const RULES: [&str; 9] = [
    UNNARROWABLE,
    SUSPICIOUS,
    BAD_CONSUMES,
    ORPHAN,
    STALE,
    STALE_ALLOW,
    STALE_ALLOW_UNNARROWABLE,
    UNCAPTURED,
    STALE_ALLOW_NO_RESULTS,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

fn severity(rule: &str) -> Severity {
    match rule {
        BAD_CONSUMES | STALE | STALE_ALLOW | STALE_ALLOW_UNNARROWABLE | STALE_ALLOW_NO_RESULTS => {
            Severity::Error
        }
        _ => Severity::Warning,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    /// Workspace-relative file, for a finding about a file.
    pub file: Option<String>,
    pub line: Option<usize>,
    /// `//project:target` (or `//project` for its configuration), for a
    /// finding about configuration.
    pub target: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub rule: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub source_files: usize,
    pub facts_reused: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Default)]
pub struct Options {
    /// Only these rules; empty runs every enabled rule.
    pub rules: Vec<String>,
    /// Target names CI requests, in addition to the configured ones.
    pub entries: Vec<String>,
    /// Where parsed facts persist (`None` parses every file).
    pub cache_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub skipped: Vec<Skipped>,
    pub stats: Stats,
}

fn known(rule: &str) -> bool {
    RULES.contains(&rule)
}

fn located(rule: &'static str, found: SourceFinding) -> Finding {
    let slashes = |p: &Path| p.to_string_lossy().replace('\\', "/");
    Finding {
        rule,
        severity: severity(rule),
        file: Some(slashes(&found.file)),
        line: found.line,
        target: found.project,
        message: found.message,
    }
}

/// Run the enabled rules over the workspace at `root`.
pub fn run(
    root: &Path,
    projects: &[DiscoveredProject],
    registry: &PluginRegistry,
    config: &WorkspaceConfig,
    options: &Options,
) -> Result<Report> {
    let started = Instant::now();
    for id in options.rules.iter().chain(&config.check.disable) {
        if !known(id) {
            bail!("unknown rule `{id}`; the rules are {}", RULES.join(", "));
        }
    }
    let enabled = |rule: &str| {
        if options.rules.is_empty() {
            !config.check.disable.iter().any(|d| d == rule)
        } else {
            options.rules.iter().any(|r| r == rule)
        }
    };
    let wrappers = &config.affected.related.wrappers;
    let mut report = Report::default();

    // Rules about targets.
    if enabled(UNNARROWABLE)
        || enabled(ORPHAN)
        || enabled(STALE_ALLOW)
        || enabled(STALE_ALLOW_UNNARROWABLE)
        || enabled(UNCAPTURED)
        || enabled(STALE_ALLOW_NO_RESULTS)
    {
        let tests = test_targets(
            projects,
            wrappers,
            registry,
            &entry_targets(config, options),
        );
        let allowed: HashSet<&str> = config
            .check
            .allow_unnarrowable
            .iter()
            .map(String::as_str)
            .collect();
        if enabled(UNNARROWABLE) {
            for test in tests.iter().filter(|t| t.unnarrowable.is_some()) {
                if allowed.contains(format!("{}:{}", test.project, test.target).as_str()) {
                    continue;
                }
                report.findings.push(Finding {
                    rule: UNNARROWABLE,
                    severity: severity(UNNARROWABLE),
                    file: None,
                    line: None,
                    target: Some(format!("{}:{}", test.project, test.target)),
                    message: format!(
                        "`{}` runs in full whenever `--related` selects it: {}",
                        test.command,
                        test.unnarrowable.as_deref().unwrap_or_default()
                    ),
                });
            }
        }
        if enabled(STALE_ALLOW_UNNARROWABLE) {
            let unnarrowable: HashSet<String> = tests
                .iter()
                .filter(|t| t.unnarrowable.is_some())
                .map(|t| format!("{}:{}", t.project, t.target))
                .collect();
            let known: HashSet<String> = tests
                .iter()
                .map(|t| format!("{}:{}", t.project, t.target))
                .collect();
            let mut listed: Vec<&String> = config.check.allow_unnarrowable.iter().collect();
            listed.sort();
            listed.dedup();
            for address in listed {
                let why = if unnarrowable.contains(address.as_str()) {
                    continue;
                } else if known.contains(address.as_str()) {
                    "names a target `--related` narrows now, so it has nothing to allow"
                } else {
                    "names no test target of the workspace"
                };
                report.findings.push(Finding {
                    rule: STALE_ALLOW_UNNARROWABLE,
                    severity: severity(STALE_ALLOW_UNNARROWABLE),
                    file: None,
                    line: None,
                    target: Some(address.clone()),
                    message: format!(
                        "`[check] allow_unnarrowable` entry `{address}` {why}; remove it"
                    ),
                });
            }
        }
        if enabled(UNCAPTURED) || enabled(STALE_ALLOW_NO_RESULTS) {
            uncaptured(projects, registry, config, &tests, &enabled, &mut report);
        }
        if enabled(ORPHAN) || enabled(STALE_ALLOW) {
            orphans(
                projects,
                config,
                options,
                &tests,
                (enabled(ORPHAN), enabled(STALE_ALLOW)),
                &mut report,
            )?;
        }
    }

    // Rules about sources and their configuration.
    let source_rules = [SUSPICIOUS, BAD_CONSUMES, STALE];
    if source_rules.iter().any(|rule| enabled(rule)) {
        let sources = Sources::load(root, options.cache_dir.as_deref());
        report.stats.source_files = sources.facts.len();
        report.stats.facts_reused = sources.reused;
        if enabled(SUSPICIOUS) {
            report.findings.extend(
                related::suspicious_path_literals(projects, &sources)
                    .into_iter()
                    .map(|f| located(SUSPICIOUS, f)),
            );
        }
        if enabled(BAD_CONSUMES) {
            report.findings.extend(
                related::bad_consumes(projects, &sources)
                    .into_iter()
                    .map(|f| located(BAD_CONSUMES, f)),
            );
        }
        if enabled(STALE) {
            report.findings.extend(
                related::stale_annotations(projects, &sources)
                    .into_iter()
                    .map(|f| located(STALE, f)),
            );
        }
    }

    let rank = |rule: &str| RULES.iter().position(|r| *r == rule).unwrap_or(RULES.len());
    report.findings.sort_by(|a, b| {
        (rank(a.rule), &a.file, a.line, &a.target, &a.message).cmp(&(
            rank(b.rule),
            &b.file,
            b.line,
            &b.target,
            &b.message,
        ))
    });
    report.stats.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

/// Test targets whose per-test results Aster cannot capture, which also
/// means `--retry-failed` cannot rerun their failing tests and a quarantined
/// test cannot be excused.
///
/// The decision is [`test_reports::plan`], the function the executor plans
/// every command with, so this rule and a run cannot disagree. A target that
/// declares its report is captured by definition. Applies to every test
/// target, entry targets included, whether or not `--related` can narrow it.
fn uncaptured(
    projects: &[DiscoveredProject],
    registry: &PluginRegistry,
    config: &WorkspaceConfig,
    tests: &[TestTarget],
    enabled: &dyn Fn(&str) -> bool,
    report: &mut Report,
) {
    let wrapper_config = &config.affected.related.wrappers;
    let wrappers = Wrappers::new(wrapper_config);
    let allowed: HashSet<&str> = config
        .check
        .allow_no_test_results
        .iter()
        .map(String::as_str)
        .collect();
    let mut missing: HashSet<String> = HashSet::new();
    for test in tests {
        let Some(project) = projects
            .iter()
            .find(|p| format!("//{}", p.relative_path.display()) == test.project)
        else {
            continue;
        };
        let Some(def) = project.targets.get(&test.target) else {
            continue;
        };
        // A target with no command runs nothing; its dependencies are the
        // test targets.
        if def.command.trim().is_empty() {
            continue;
        }
        let Some(plugin) = registry.find_by_name(&project.plugin_name) else {
            continue;
        };
        let Ok(report_dir) = new_report_dir() else {
            continue;
        };
        let command = def.command.replace("{files}", "");
        let working_dir = def.working_dir.as_deref().unwrap_or(&project.root);
        let context = ReportContext {
            project_dir: &project.root,
            working_dir,
            target_name: &test.target,
            command: command.trim(),
            report_dir: &report_dir,
        };
        let planned = test_reports::plan(plugin, def.report.as_ref(), &context, &wrappers);
        let _ = std::fs::remove_dir_all(&report_dir);
        if planned.is_some() {
            continue;
        }
        let address = format!("{}:{}", test.project, test.target);
        missing.insert(address.clone());
        if enabled(UNCAPTURED) && !allowed.contains(address.as_str()) {
            let reason = uncaptured_reason(projects, wrapper_config, registry, project, def);
            report.findings.push(Finding {
                rule: UNCAPTURED,
                severity: severity(UNCAPTURED),
                file: None,
                line: None,
                target: Some(address),
                message: format!(
                    "Aster cannot capture per-test results from `{}`, so `--retry-failed` and \
                     quarantine do nothing for it: {reason}",
                    test.command
                ),
            });
        }
    }
    if enabled(STALE_ALLOW_NO_RESULTS) {
        let known: HashSet<String> = tests
            .iter()
            .map(|t| format!("{}:{}", t.project, t.target))
            .collect();
        let mut listed: Vec<&String> = config.check.allow_no_test_results.iter().collect();
        listed.sort();
        listed.dedup();
        for address in listed {
            let why = if missing.contains(address.as_str()) {
                continue;
            } else if known.contains(address.as_str()) {
                "names a target whose results Aster captures now, so it has nothing to allow"
            } else {
                "names no test target of the workspace"
            };
            report.findings.push(Finding {
                rule: STALE_ALLOW_NO_RESULTS,
                severity: severity(STALE_ALLOW_NO_RESULTS),
                file: None,
                line: None,
                target: Some(address.clone()),
                message: format!(
                    "`[check] allow_no_test_results` entry `{address}` {why}; remove it"
                ),
            });
        }
    }
}

type Suites = Vec<(String, Option<String>)>;

/// The CI entry targets: `[check] entries`, `--entry`, and the `target` of
/// each lane.
fn entry_targets(config: &WorkspaceConfig, options: &Options) -> Vec<String> {
    let mut entries: Vec<String> = config.check.entries.clone();
    entries.extend(options.entries.iter().cloned());
    entries.extend(
        config
            .affected
            .lanes
            .values()
            .filter_map(|lane| lane.target.clone()),
    );
    entries.sort();
    entries.dedup();
    entries
}

/// Test targets that no entry target runs.
fn orphans(
    projects: &[DiscoveredProject],
    config: &WorkspaceConfig,
    options: &Options,
    tests: &[crate::cli::affected_files::TestTarget],
    (report_orphans, report_stale_allows): (bool, bool),
    report: &mut Report,
) -> Result<()> {
    let lanes = &config.affected.lanes;
    if lanes.is_empty() {
        report.skipped.push(Skipped {
            rule: ORPHAN,
            reason: "the workspace has no [affected.lanes], so there is no CI entry to reach \
                     targets from"
                .into(),
        });
        return Ok(());
    }
    let mut global: Vec<String> = config.check.entries.clone();
    global.extend(options.entries.iter().cloned());
    global.sort();
    global.dedup();
    let any_lane_target = lanes.values().any(|lane| lane.target.is_some());
    if global.is_empty() && !any_lane_target {
        report.skipped.push(Skipped {
            rule: ORPHAN,
            reason: "no entry target is known: set `[check] entries`, pass `--entry <target>`, \
                     or give a lane a `target`"
                .into(),
        });
        return Ok(());
    }
    let all: HashSet<String> = projects
        .iter()
        .map(|p| format!("//{}", p.relative_path.display()))
        .collect();
    let mut admitted: HashSet<String> = HashSet::new();
    let mut roots: Vec<(String, String)> = Vec::new();
    let mut names: Vec<&String> = lanes.keys().collect();
    names.sort();
    for name in names {
        let lane = &lanes[name];
        let members = select_affected_lane(name, &config.affected, all.clone(), projects)?;
        let entries: Vec<String> = match &lane.target {
            Some(target) => vec![target.clone()],
            None => global.clone(),
        };
        for project in &members {
            for entry in &entries {
                roots.push((project.clone(), entry.clone()));
            }
        }
        admitted.extend(members);
    }
    let roots_by_addr: HashMap<String, PathBuf> = projects
        .iter()
        .map(|p| (format!("//{}", p.relative_path.display()), p.root.clone()))
        .collect();
    let reached = reachable_targets(projects, &config.affected.related.wrappers, &roots);
    // A whole-suite run of a project's tests (a shard that changes into the
    // project, say) covers the project's other targets that run the same
    // runner on some of its files.
    // Each is the project it runs in and its `(program, config)` suites.
    let mut whole: Vec<(String, Suites)> = tests
        .iter()
        .filter(|t| {
            !t.whole_suites.is_empty() && reached.contains(&(t.project.clone(), t.target.clone()))
        })
        .map(|t| (t.runs_in.clone(), t.whole_suites.clone()))
        .collect();
    // Package scripts and `{files}` commands run a suite their words do not
    // name.
    whole.extend(
        suite_runs(projects, &config.affected.related.wrappers)
            .into_iter()
            .filter(|s| reached.contains(&(s.project.clone(), s.target.clone())))
            .map(|s| (s.runs_in, s.suites)),
    );
    // A reached `cargo test --workspace` or `cargo nextest run --workspace`
    // runs every member crate's tests, under either runner.
    let workspaces: Vec<PathBuf> = tests
        .iter()
        .filter(|t| {
            reached.contains(&(t.project.clone(), t.target.clone()))
                && t.is_cargo_test()
                && t.whole_suite_flags()
                && !t.selects_cargo_tests()
        })
        .filter_map(|t| roots_by_addr.get(&t.runs_in).cloned())
        .collect();
    // A script that a reached target calls (`pnpm test-ci` where `test-ci` is
    // `pnpm a && pnpm b`) runs, and so does the target that is that script.
    let called = called_scripts(projects, &config.affected.related.wrappers, &reached);
    let allowed: HashSet<&str> = config
        .check
        .allow_orphans
        .iter()
        .map(String::as_str)
        .collect();
    let mut orphaned: HashSet<String> = HashSet::new();
    for test in tests {
        if reached.contains(&(test.project.clone(), test.target.clone())) {
            continue;
        }
        if test.is_cargo_test()
            && !test.selective
            && !test.selects_cargo_tests()
            && roots_by_addr
                .get(&test.runs_in)
                .is_some_and(|root| workspaces.iter().any(|w| root.starts_with(w)))
        {
            continue;
        }
        if test
            .script
            .as_ref()
            .is_some_and(|script| called.contains(&(test.runs_in.clone(), script.clone())))
        {
            continue;
        }
        // An alias of scripts that run (`test` = `pnpm test-ci`) runs nothing
        // of its own.
        if test.script_aliases.as_ref().is_some_and(|names| {
            !names.is_empty()
                && names
                    .iter()
                    .all(|name| called.contains(&(test.runs_in.clone(), name.clone())))
        }) {
            continue;
        }
        if !test.selective
            && whole.iter().any(|(runs_in, suites)| {
                *runs_in == test.runs_in
                    && suites
                        .iter()
                        .any(|(program, config)| *program == test.program && *config == test.config)
            })
        {
            continue;
        }
        let address = format!("{}:{}", test.project, test.target);
        orphaned.insert(address.clone());
        if allowed.contains(address.as_str()) || !report_orphans {
            continue;
        }
        let why = if admitted.contains(&test.project) {
            "no entry target depends on it or is joined by it with `selected_by`"
        } else {
            "its project is in no [affected.lanes] entry"
        };
        report.findings.push(Finding {
            rule: ORPHAN,
            severity: severity(ORPHAN),
            file: None,
            line: None,
            target: Some(address),
            message: format!("`{}` never runs automatically: {why}", test.command),
        });
    }
    if report_stale_allows {
        let tests_by_address: HashSet<String> = tests
            .iter()
            .map(|t| format!("{}:{}", t.project, t.target))
            .collect();
        let mut listed: Vec<&String> = config.check.allow_orphans.iter().collect();
        listed.sort();
        listed.dedup();
        for address in listed {
            let why = if !tests_by_address.contains(address.as_str()) {
                "names no test target of the workspace"
            } else if !orphaned.contains(address.as_str()) {
                "names a target an entry already reaches, so it is not an orphan"
            } else {
                continue;
            };
            report.findings.push(Finding {
                rule: STALE_ALLOW,
                severity: severity(STALE_ALLOW),
                file: None,
                line: None,
                target: Some(address.clone()),
                message: format!("`[check] allow_orphans` entry `{address}` {why}; remove it"),
            });
        }
    }
    Ok(())
}

impl Report {
    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count()
    }

    /// `0` for no findings or only warnings, `1` for an error finding, or
    /// for any finding with `strict`.
    pub fn exit_code(&self, strict: bool) -> i32 {
        i32::from(self.errors() > 0 || (strict && !self.findings.is_empty()))
    }

    fn by_rule(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for finding in &self.findings {
            *counts.entry(finding.rule).or_insert(0) += 1;
        }
        counts
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "findings": self.findings,
            "summary": {
                "findings": self.findings.len(),
                "errors": self.errors(),
                "warnings": self.findings.len() - self.errors(),
                "by_rule": self.by_rule(),
            },
            "skipped": self.skipped,
            "stats": self.stats,
        })
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for finding in &self.findings {
            let place = match (&finding.file, finding.line, &finding.target) {
                (Some(file), Some(line), _) => format!("{file}:{line}"),
                (Some(file), None, Some(target)) => format!("{file} ({target})"),
                (Some(file), None, None) => file.clone(),
                (None, _, Some(target)) => target.clone(),
                (None, _, None) => String::new(),
            };
            let level = match finding.severity {
                Severity::Warning => "warning",
                Severity::Error => "error",
            };
            let _ = writeln!(
                out,
                "{place}: {level}[{}] {}",
                finding.rule, finding.message
            );
        }
        for skipped in &self.skipped {
            let _ = writeln!(out, "note: skipped {}: {}", skipped.rule, skipped.reason);
        }
        if self.findings.is_empty() {
            let _ = writeln!(out, "aster check: no findings");
        } else {
            let counts: Vec<String> = self
                .by_rule()
                .iter()
                .map(|(rule, n)| format!("{rule} {n}"))
                .collect();
            let _ = writeln!(
                out,
                "aster check: {} finding(s), {} error(s) ({})",
                self.findings.len(),
                self.errors(),
                counts.join(", ")
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::discover_projects;

    /// `uncaptured-test-results` and the executor ask the same function, so a
    /// target is reported exactly when a run would record no tests for it.
    /// Plans every test target through the executor's own `plan_captures`
    /// and compares with the rule.
    #[test]
    fn the_rule_agrees_with_the_executors_report_planning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let write = |path: &str, content: &str| {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        };
        write(
            "aster.toml",
            "[check]\nentries = [\"test-script\", \"test-declared\"]\n\n\
             [affected.related]\nwrappers = [\"with-rust-toolchain\", { program = \"mise\", until = \"--\" }]\n",
        );
        write(
            "web/package.json",
            r#"{"name":"web","scripts":{"lint":"eslint ."}}"#,
        );
        write(
            "web/aster.toml",
            r#"
[targets.test-plain]
command = "bun test"
[targets.test-wrapped]
command = "mise exec go -- bun test --timeout=60000"
[targets.test-unlisted]
command = "unlisted exec -- bun test"
[targets.test-script]
command = "./run-tests.sh"
[targets.test-compound]
command = "bash -c 'bun test && bun run lint'"
[targets.test-declared]
command = "./run-tests.sh"
[targets.test-declared.report]
format = "junit"
"#,
        );
        write("svc/go.mod", "module example.com/svc\n\ngo 1.21\n");
        write(
            "svc/aster.toml",
            "[targets.test-go]\ncommand = \"mise exec go -- go test ./...\"\n\n\
             [targets.test-go-bare]\ncommand = \"go test ./...\"\n",
        );
        write(
            "rs/Cargo.toml",
            "[package]\nname = \"rs\"\nversion = \"0.1.0\"\n",
        );
        write(
            "rs/aster.toml",
            "[targets.test-rs]\ncommand = \"with-rust-toolchain cargo test\"\n",
        );

        let registry = PluginRegistry::with_all_plugins();
        let projects = discover_projects(root, &registry).unwrap();
        let config = WorkspaceConfig::load(root).unwrap();
        let options = Options {
            rules: vec![UNCAPTURED.to_string()],
            ..Options::default()
        };
        let report = run(root, &projects, &registry, &config, &options).unwrap();
        let flagged: HashSet<String> = report
            .findings
            .iter()
            .filter_map(|f| f.target.clone())
            .collect();

        let wrappers = Wrappers::new(&config.affected.related.wrappers);
        let tests = test_targets(
            &projects,
            &config.affected.related.wrappers,
            &registry,
            &entry_targets(&config, &options),
        );
        assert!(tests.len() >= 9, "{tests:?}");
        for test in &tests {
            let project = projects
                .iter()
                .find(|p| format!("//{}", p.relative_path.display()) == test.project)
                .unwrap();
            let def = &project.targets[&test.target];
            let captures = crate::executor::runner::plan_captures(
                registry.find_by_name(&project.plugin_name).unwrap(),
                def.report.as_ref(),
                &project.root,
                &project.root,
                &test.target,
                std::slice::from_ref(&def.command),
                &wrappers,
            );
            let planned = captures[0].is_some();
            for (_, dir) in captures.iter().flatten() {
                let _ = std::fs::remove_dir_all(dir);
            }
            let address = format!("{}:{}", test.project, test.target);
            assert_eq!(
                flagged.contains(&address),
                !planned,
                "{address} `{}`: rule and executor disagree",
                test.command
            );
        }
        // And the cases are the ones the rule is about.
        for (address, is_flagged) in [
            ("//web:test-plain", false),
            ("//web:test-wrapped", false),
            ("//web:test-declared", false),
            ("//web:test-unlisted", true),
            ("//web:test-script", true),
            ("//web:test-compound", true),
            ("//svc:test-go", false),
            ("//svc:test-go-bare", false),
            ("//rs:test-rs", false),
        ] {
            assert_eq!(
                flagged.contains(address),
                is_flagged,
                "{address}: {flagged:?}"
            );
        }
    }
}
