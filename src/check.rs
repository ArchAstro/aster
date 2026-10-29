//! `aster check`: a lint of the configuration and annotations that
//! `aster affected --related` depends on.
//!
//! Every rule has a stable id. Findings are warnings or errors; the command
//! changes no selection. See the README, "Checking the configuration".

use crate::cli::affected_files::{reachable_targets, test_targets};
use crate::config::WorkspaceConfig;
use crate::discovery::DiscoveredProject;
use crate::git::select_affected_lane;
use crate::plugins::PluginRegistry;
use crate::related::{self, SourceFinding, Sources};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::path::Path;
use std::time::Instant;

pub const UNNARROWABLE: &str = "unnarrowable-test-command";
pub const SUSPICIOUS: &str = "suspicious-path-literal";
pub const BAD_CONSUMES: &str = "bad-consumes";
pub const ORPHAN: &str = "orphan-test-target";
pub const STALE: &str = "stale-annotation";

/// Every rule id, in the order findings are listed.
pub const RULES: [&str; 5] = [UNNARROWABLE, SUSPICIOUS, BAD_CONSUMES, ORPHAN, STALE];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

fn severity(rule: &str) -> Severity {
    match rule {
        BAD_CONSUMES | STALE => Severity::Error,
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
    if enabled(UNNARROWABLE) || enabled(ORPHAN) {
        let tests = test_targets(projects, wrappers, registry);
        if enabled(UNNARROWABLE) {
            for test in tests.iter().filter(|t| t.unnarrowable.is_some()) {
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
        if enabled(ORPHAN) {
            orphans(projects, config, options, &tests, &mut report)?;
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

/// Test targets that no entry target runs.
fn orphans(
    projects: &[DiscoveredProject],
    config: &WorkspaceConfig,
    options: &Options,
    tests: &[crate::cli::affected_files::TestTarget],
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
    let reached = reachable_targets(projects, &config.affected.related.wrappers, &roots);
    // A whole-suite run of a project's tests (a shard that changes into the
    // project, say) covers the project's other targets that run the same
    // runner on some of its files.
    let whole: Vec<&crate::cli::affected_files::TestTarget> = tests
        .iter()
        .filter(|t| t.whole_suite && reached.contains(&(t.project.clone(), t.target.clone())))
        .collect();
    for test in tests {
        if reached.contains(&(test.project.clone(), test.target.clone())) {
            continue;
        }
        if !test.selective
            && whole
                .iter()
                .any(|w| w.runs_in == test.runs_in && w.program == test.program)
        {
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
            target: Some(format!("{}:{}", test.project, test.target)),
            message: format!("`{}` never runs automatically: {why}", test.command),
        });
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
