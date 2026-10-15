//! `aster results summary`: what failed in a results file, labelled with
//! what aster-srv knows, as a Markdown report and GitHub annotations.
//!
//! A test failed when its last attempt failed or errored; it is flaky on
//! retry when an earlier attempt failed and the last passed. Each failing
//! test is looked up on the server (when there is one) and labelled:
//!
//! - `known flake`: the server classifies it flaky
//! - `broken`: the server classifies it broken, since some commit
//! - `new`: the server calls it healthy, or has never seen it
//! - `failed`: no label, because the server could not be asked
//!
//! A failing test that Aster let through because it is quarantined is
//! listed as `quarantined` and not looked up.
//!
//! The report never fails the build: a lookup problem only costs labels.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use aster_results_model::{parse_ndjson, Record, TestRecord};
use serde_json::Value;

use super::client::{percent_encode, Server};
use super::upload::{proxy_token_from_env, Credentials};

/// The most tests looked up on the server in one report.
const MAX_LOOKUPS: usize = 50;
/// The most rows in the Markdown table.
const MAX_ROWS: usize = 200;
/// Longest message shown in a table cell, in characters.
const CELL_MESSAGE_CHARS: usize = 160;
/// Longest message carried by an annotation, in characters.
const ANNOTATION_MESSAGE_CHARS: usize = 2000;

/// What the report says about one test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    /// No server answer to go on.
    Failed,
    /// Healthy or unknown to the server: this failure is news.
    New,
    Broken {
        since_sha: Option<String>,
    },
    KnownFlake,
    /// Failed, but quarantined: its target passed anyway.
    Quarantined,
    /// Failed, then passed on retry.
    FlakyOnRetry,
}

impl Label {
    /// The word used in tables and annotation titles.
    pub fn text(&self) -> &'static str {
        match self {
            Label::Failed => "failed",
            Label::New => "new",
            Label::Broken { .. } => "broken",
            Label::KnownFlake => "known flake",
            Label::Quarantined => "quarantined",
            Label::FlakyOnRetry => "flaky on retry",
        }
    }

    /// Position in the report: new first, then broken, known flakes,
    /// quarantined tests, and the tests that passed on retry last.
    fn rank(&self) -> u8 {
        match self {
            Label::New => 0,
            Label::Failed => 1,
            Label::Broken { .. } => 2,
            Label::KnownFlake => 3,
            Label::Quarantined => 4,
            Label::FlakyOnRetry => 5,
        }
    }

    /// Whether the test failed its target. Quarantined and flaky-on-retry
    /// tests did not.
    fn fails(&self) -> bool {
        !matches!(self, Label::Quarantined | Label::FlakyOnRetry)
    }
}

/// One test in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub test_id: String,
    pub name: String,
    pub target: String,
    /// Relative to the target's project directory.
    pub file: Option<String>,
    pub message: Option<String>,
    pub label: Label,
}

/// A results file, read for its failures.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub repo: Option<String>,
    /// Failing tests and flaky-on-retry tests; see [`label`] and [`sort`].
    pub entries: Vec<Entry>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn count(&self, rank: u8) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.label.rank() == rank)
            .count()
    }

    fn failing(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|entry| entry.label.fails())
    }
}

/// Read a results file. Failing tests start as [`Label::Failed`].
pub fn read(path: &Path) -> Result<Report> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let records =
        parse_ndjson(&text).with_context(|| format!("{} is not a results file", path.display()))?;
    Ok(collect(&records))
}

/// The failing and flaky-on-retry tests among `records`, in file order.
pub fn collect(records: &[Record]) -> Report {
    let mut repo = None;
    // Attempts of each test, in file order, keyed by target and test id.
    let mut order: Vec<(&str, &str)> = Vec::new();
    let mut attempts: HashMap<(&str, &str), Vec<&TestRecord>> = HashMap::new();
    for record in records {
        match record {
            Record::Run(run) => repo = Some(run.repo.clone()),
            Record::Test(test) => {
                let key = (test.target.as_str(), test.test_id.as_str());
                attempts
                    .entry(key)
                    .or_insert_with(|| {
                        order.push(key);
                        Vec::new()
                    })
                    .push(test);
            }
            Record::Target(_) => {}
        }
    }
    let mut entries = Vec::new();
    for key in order {
        let tests = &attempts[&key];
        let last = tests
            .iter()
            .max_by_key(|test| test.attempt)
            .expect("a test has an attempt");
        let label = if last.outcome.is_failure() && last.quarantined {
            Label::Quarantined
        } else if last.outcome.is_failure() {
            Label::Failed
        } else if last.outcome == aster_results_model::TestOutcome::Pass
            && tests.iter().any(|test| test.outcome.is_failure())
        {
            Label::FlakyOnRetry
        } else {
            continue;
        };
        // The message of the latest failing attempt.
        let message = tests
            .iter()
            .filter(|test| test.outcome.is_failure())
            .max_by_key(|test| test.attempt)
            .and_then(|test| test.message.clone())
            .filter(|message| !message.trim().is_empty());
        entries.push(Entry {
            test_id: last.test_id.clone(),
            name: last.name.clone(),
            target: last.target.clone(),
            file: last.file.clone().filter(|file| !file.is_empty()),
            message,
            label,
        });
    }
    Report { repo, entries }
}

/// What looking tests up on the server came to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lookups {
    /// Why some failures have no label, when that is so.
    pub problem: Option<String>,
}

/// Label the failing entries with `lookup` (a test id to the server's JSON
/// for it, `None` for 404). At most 50 are made, and none after
/// the first error: the rest stay `failed`.
pub fn label(
    report: &mut Report,
    lookup: &mut dyn FnMut(&str) -> Result<Option<Value>>,
) -> Lookups {
    let mut made = 0;
    let mut problem = None;
    let mut skipped = 0;
    for entry in &mut report.entries {
        if entry.label != Label::Failed {
            continue;
        }
        if problem.is_some() {
            skipped += 1;
            continue;
        }
        if made == MAX_LOOKUPS {
            skipped += 1;
            continue;
        }
        made += 1;
        match lookup(&entry.test_id) {
            Ok(found) => entry.label = label_from(found.as_ref()),
            Err(error) => {
                problem = Some(format!("{error:#}"));
                skipped += 1;
            }
        }
    }
    if problem.is_none() && skipped > 0 {
        problem = Some(format!(
            "only the first {MAX_LOOKUPS} failures were looked up"
        ));
    }
    Lookups { problem }
}

/// The label for the server's answer about a failing test.
fn label_from(found: Option<&Value>) -> Label {
    let test = found.map(|found| &found["test"]);
    match test.and_then(|test| test["state"].as_str()) {
        Some("flaky") => Label::KnownFlake,
        Some("broken") => Label::Broken {
            since_sha: test
                .and_then(|test| test["broken_since_sha"].as_str())
                .map(str::to_string),
        },
        _ => Label::New,
    }
}

/// Put the entries in report order: by label, then as in the file.
pub fn sort(report: &mut Report) {
    report.entries.sort_by_key(|entry| entry.label.rank());
}

/// `//services/platform:test-ci` is project `services/platform`; `//:x` is
/// the workspace root (empty).
fn project_dir(target: &str) -> &str {
    let address = target.strip_prefix("//").unwrap_or(target);
    address.rsplit_once(':').map_or(address, |(dir, _)| dir)
}

/// The workspace-relative path of an entry's file.
pub fn annotation_path(entry: &Entry) -> Option<String> {
    let file = entry.file.as_deref()?.replace('\\', "/");
    let file = file.trim_start_matches("./");
    let dir = project_dir(&entry.target).trim_matches('/');
    Some(if dir.is_empty() || file.starts_with('/') {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    })
}

/// The line a failure message points at in `file`: the first
/// `<basename>:<digits>`, where the basename does not continue a longer
/// name.
pub fn failure_line(file: &str, message: &str) -> Option<u32> {
    let base = file.rsplit(['/', '\\']).next().filter(|b| !b.is_empty())?;
    let needle = format!("{base}:");
    for (at, _) in message.match_indices(&needle) {
        let continues_name = message[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || "_-.".contains(c));
        if continues_name {
            continue;
        }
        let digits: String = message[at + needle.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(line) = digits.parse::<u32>() {
            if line > 0 {
                return Some(line);
            }
        }
    }
    None
}

/// Escape a workflow command's message.
pub fn escape_data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a workflow command's property value.
pub fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}

fn truncate(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The workflow commands for the report: an error per failing test and, when
/// `flaky` is set, a warning per flaky-on-retry or quarantined test.
pub fn annotations(report: &Report, flaky: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in &report.entries {
        let flaky_on_retry = entry.label == Label::FlakyOnRetry;
        let warned = !entry.label.fails();
        if warned && !flaky {
            continue;
        }
        let level = if warned { "warning" } else { "error" };
        let mut properties = Vec::new();
        if let Some(path) = annotation_path(entry) {
            properties.push(format!("file={}", escape_property(&path)));
            if let Some(line) = entry
                .file
                .as_deref()
                .zip(entry.message.as_deref())
                .and_then(|(file, message)| failure_line(file, message))
            {
                properties.push(format!("line={line}"));
            }
        }
        properties.push(format!(
            "title={}",
            escape_property(&format!("{}: {}", entry.label.text(), entry.name))
        ));
        let message = if flaky_on_retry {
            format!("{} failed then passed on retry", entry.name)
        } else {
            let text = entry
                .message
                .as_deref()
                .map(|m| truncate(m.trim(), ANNOTATION_MESSAGE_CHARS))
                .unwrap_or_else(|| format!("{} failed", entry.name));
            match &entry.label {
                Label::Broken {
                    since_sha: Some(sha),
                } => format!("broken since {sha}\n{text}"),
                Label::Quarantined => format!("quarantined, so its target passed\n{text}"),
                _ => text,
            }
        };
        lines.push(format!(
            "::{level} {}::{}",
            properties.join(","),
            escape_data(&message)
        ));
    }
    lines
}

/// Escape text for a Markdown table cell.
fn cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' | '|' | '`' | '*' | '_' | '[' | ']' | '<' | '>' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Inline code for a table cell; `|` still ends the cell, so it is escaped.
fn code(text: &str) -> String {
    format!(
        "<code>{}</code>",
        text.replace('|', "&#124;").replace('<', "&lt;")
    )
}

/// The Markdown report. `url` is the aster-srv base URL, when the test
/// names should link to its pages. `problem` is why labels are missing.
pub fn markdown(report: &Report, url: Option<&str>, problem: Option<&str>) -> String {
    let failing = report.failing().count();
    let flaky = report.count(Label::FlakyOnRetry.rank());
    let quarantined = report.count(Label::Quarantined.rank());
    let mut out = String::from("## Aster test results\n\n");
    if report.is_empty() {
        out.push_str("No failing tests.\n");
        return out;
    }
    let mut counts = vec![format!(
        "**{failing} failing test{}**",
        if failing == 1 { "" } else { "s" }
    )];
    for (rank, text) in [(0, "new"), (2, "broken"), (3, "known flake")] {
        let n = report.count(rank);
        if n > 0 {
            counts.push(format!("{n} {text}"));
        }
    }
    let unlabelled = report.count(Label::Failed.rank());
    if unlabelled > 0 && unlabelled < failing {
        counts.push(format!("{unlabelled} unlabelled"));
    }
    if quarantined > 0 {
        counts.push(format!(
            "{quarantined} quarantined (did not fail the build)"
        ));
    }
    if flaky > 0 {
        counts.push(format!("{flaky} flaky on retry"));
    }
    out.push_str(&counts.join(" · "));
    out.push_str("\n\n");

    out.push_str("| | Test | File | Target | Message |\n|---|---|---|---|---|\n");
    for entry in report.entries.iter().take(MAX_ROWS) {
        let name = match (url, &report.repo) {
            (Some(url), Some(repo)) => format!(
                "[{}]({}/ui/tests/{}?repo={})",
                cell(&entry.name),
                url.trim_end_matches('/'),
                percent_encode(&entry.test_id),
                percent_encode(repo)
            ),
            _ => cell(&entry.name),
        };
        let label = match &entry.label {
            Label::Broken {
                since_sha: Some(sha),
            } => format!("broken since {}", cell(&sha[..sha.len().min(10)])),
            other => other.text().to_string(),
        };
        let message = entry
            .message
            .as_deref()
            .and_then(|m| m.lines().find(|line| !line.trim().is_empty()))
            .map(|line| cell(&truncate(line.trim(), CELL_MESSAGE_CHARS)))
            .unwrap_or_default();
        out.push_str(&format!(
            "| {label} | {name} | {} | {} | {message} |\n",
            entry.file.as_deref().map(code).unwrap_or_default(),
            code(&entry.target),
        ));
    }
    if report.entries.len() > MAX_ROWS {
        out.push_str(&format!(
            "\n…and {} more.\n",
            report.entries.len() - MAX_ROWS
        ));
    }
    if let Some(problem) = problem {
        out.push_str(&format!(
            "\n_Some failures are not labelled new, broken or known flake: {}._\n",
            cell(problem)
        ));
    }
    out
}

/// How to run a summary.
pub struct Options<'a> {
    pub path: &'a Path,
    /// The aster-srv base URL; without one nothing is labelled.
    pub url: Option<String>,
    /// Append to `$GITHUB_STEP_SUMMARY` and print annotations instead of
    /// printing the Markdown.
    pub github: bool,
    /// Also warn about tests that passed on retry. Aster has already warned
    /// during a run that retried them, so the automatic summary leaves
    /// these out.
    pub flaky_warnings: bool,
}

/// Build the report, and print or append it. `out` receives the Markdown
/// (without `--github`) or the workflow commands (with it); `env` looks up
/// environment variables.
pub fn run(
    options: &Options,
    env: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn Write,
) -> Result<()> {
    let mut report = read(options.path)?;
    let mut problem = None;
    if report.entries.iter().any(|e| e.label == Label::Failed) {
        problem = match lookup_server(options, env) {
            Ok(Some(server)) => label(&mut report, &mut |id| server.test(id)).problem,
            Ok(None) => None,
            Err(error) => Some(format!("{error:#}")),
        };
    }
    sort(&mut report);

    if !options.github {
        out.write_all(markdown(&report, options.url.as_deref(), problem.as_deref()).as_bytes())?;
        return Ok(());
    }
    if report.is_empty() {
        return Ok(());
    }
    if let Some(summary) = env("GITHUB_STEP_SUMMARY").filter(|path| !path.is_empty()) {
        let text = markdown(&report, options.url.as_deref(), problem.as_deref());
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary)
            .with_context(|| format!("cannot open {summary}"))?;
        writeln!(file, "{text}")?;
    }
    for line in annotations(&report, options.flaky_warnings) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// A server connection when the URL and credentials are there. A URL
/// without credentials is not an error: the report just has no labels.
fn lookup_server(
    options: &Options,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Server>> {
    let Some(url) = options.url.as_deref() else {
        return Ok(None);
    };
    let Some(credentials) = Credentials::from_env(env) else {
        anyhow::bail!("no credentials for {url}");
    };
    Server::connect(url, &credentials, proxy_token_from_env(env)).map(Some)
}

#[cfg(test)]
mod tests {
    use super::super::client::testing::{closed_port, serve};
    use super::*;
    use aster_results_model::{RunRecord, TestOutcome};

    fn run_record() -> Record {
        Record::Run(RunRecord {
            run_id: "gh:a/b:1:1:j:-:-:00".into(),
            repo: "a/b".into(),
            sha: "abc".into(),
            branch: None,
            pr: None,
            workflow: None,
            job: None,
            attempt: None,
            lane: None,
            runner: None,
            aster_version: "0".into(),
            started_at: "2026-10-13T00:00:00Z".into(),
        })
    }

    fn test(id: &str, outcome: TestOutcome, attempt: u32, message: Option<&str>) -> Record {
        Record::Test(TestRecord {
            target: "//services/platform:test-ci".into(),
            test_id: id.into(),
            file: Some(format!("test/{id}_test.exs")),
            suite: None,
            name: format!("test {id}"),
            outcome,
            attempt,
            duration_ms: None,
            failure_hash: None,
            message: message.map(str::to_string),
            quarantined: false,
        })
    }

    #[test]
    fn quarantined_failures_are_listed_but_not_failures() {
        let Record::Test(mut held) = test("held", TestOutcome::Fail, 1, Some("boom")) else {
            unreachable!()
        };
        held.quarantined = true;
        let mut report = collect(&[
            run_record(),
            Record::Test(held),
            test("real", TestOutcome::Fail, 1, Some("bad")),
        ]);
        assert_eq!(report.entries[0].label, Label::Quarantined);
        // Only the real failure is looked up.
        let mut asked = Vec::new();
        label(&mut report, &mut |id| {
            asked.push(id.to_string());
            Ok(None)
        });
        assert_eq!(asked, ["real"]);
        sort(&mut report);
        let text = markdown(&report, None, None);
        assert!(text.contains("**1 failing test**"), "{text}");
        assert!(
            text.contains("1 quarantined (did not fail the build)"),
            "{text}"
        );
        assert!(text.contains("| quarantined | test held"), "{text}");
        let errors = annotations(&report, false);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("::error"));
        let all = annotations(&report, true);
        assert!(all[1].starts_with("::warning") && all[1].contains("quarantined"));
    }

    fn sample() -> Vec<Record> {
        use TestOutcome::*;
        vec![
            run_record(),
            test("ok", Pass, 1, None),
            test("skipped", Skip, 1, None),
            test("fails", Fail, 1, Some("boom\nat fails_test.exs:42")),
            test("errors", Error, 1, Some("crashed")),
            test("flaky", Fail, 1, Some("first try")),
            test("flaky", Pass, 2, None),
            test("still", Fail, 1, Some("first")),
            test("still", Fail, 2, Some("second")),
        ]
    }

    #[test]
    fn collects_final_failures_and_flaky_on_retry() {
        let report = collect(&sample());
        assert_eq!(report.repo.as_deref(), Some("a/b"));
        let seen: Vec<(&str, &Label)> = report
            .entries
            .iter()
            .map(|e| (e.test_id.as_str(), &e.label))
            .collect();
        assert_eq!(
            seen,
            [
                ("fails", &Label::Failed),
                ("errors", &Label::Failed),
                ("flaky", &Label::FlakyOnRetry),
                ("still", &Label::Failed),
            ]
        );
        let still = &report.entries[3];
        assert_eq!(still.message.as_deref(), Some("second"));
        assert_eq!(report.entries[2].message.as_deref(), Some("first try"));
    }

    fn found(state: &str, since: Option<&str>) -> Option<Value> {
        Some(serde_json::json!({"test": {"state": state, "broken_since_sha": since}}))
    }

    #[test]
    fn labels_from_the_servers_state_and_404() {
        let mut report = collect(&sample());
        let mut asked = Vec::new();
        let lookups = label(&mut report, &mut |id| {
            asked.push(id.to_string());
            Ok(match id {
                "fails" => found("flaky", None),
                "errors" => found("broken", Some("deadbeef")),
                _ => None,
            })
        });
        assert_eq!(lookups, Lookups { problem: None });
        assert_eq!(
            asked,
            ["fails", "errors", "still"],
            "flaky-on-retry is not asked"
        );
        sort(&mut report);
        let labels: Vec<&Label> = report.entries.iter().map(|e| &e.label).collect();
        assert_eq!(
            labels,
            [
                &Label::New,
                &Label::Broken {
                    since_sha: Some("deadbeef".into())
                },
                &Label::KnownFlake,
                &Label::FlakyOnRetry
            ]
        );
        assert_eq!(label_from(found("healthy", None).as_ref()), Label::New);
    }

    #[test]
    fn stops_looking_up_after_the_first_error() {
        let mut report = collect(&sample());
        let mut calls = 0;
        let lookups = label(&mut report, &mut |_| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("503")
            }
            Ok(found("flaky", None))
        });
        assert_eq!(calls, 2);
        assert_eq!(lookups.problem.as_deref(), Some("503"));
        let labels: Vec<&str> = report.entries.iter().map(|e| e.label.text()).collect();
        assert_eq!(
            labels,
            ["known flake", "failed", "flaky on retry", "failed"]
        );
    }

    #[test]
    fn caps_lookups() {
        let mut records = vec![run_record()];
        for i in 0..MAX_LOOKUPS + 5 {
            records.push(test(&format!("t{i}"), TestOutcome::Fail, 1, None));
        }
        let mut report = collect(&records);
        let mut calls = 0;
        let lookups = label(&mut report, &mut |_| {
            calls += 1;
            Ok(None)
        });
        assert_eq!(calls, MAX_LOOKUPS);
        assert!(lookups.problem.unwrap().contains("first 50"));
        assert_eq!(report.count(Label::Failed.rank()), 5);
    }

    #[test]
    fn escapes_workflow_command_text() {
        assert_eq!(escape_data("100%\r\na:b,c"), "100%25%0D%0Aa:b,c");
        assert_eq!(escape_property("a:b,c%\n"), "a%3Ab%2Cc%25%0A");
    }

    #[test]
    fn finds_the_project_path_and_the_line() {
        let entry = |target: &str, file: Option<&str>| Entry {
            test_id: "x".into(),
            name: "n".into(),
            target: target.into(),
            file: file.map(str::to_string),
            message: None,
            label: Label::Failed,
        };
        let path = |target, file| annotation_path(&entry(target, file));
        assert_eq!(
            path("//services/platform:test-ci", Some("test/a_test.exs")).as_deref(),
            Some("services/platform/test/a_test.exs")
        );
        assert_eq!(path("//:test", Some("./t/a.go")).as_deref(), Some("t/a.go"));
        assert_eq!(path("//app:test", None), None);

        let message = "** (RuntimeError) in other_a_test.exs:7\n    test/a_test.exs:42: A.run/0\n  a_test.exs:99";
        assert_eq!(failure_line("test/a_test.exs", message), Some(42));
        assert_eq!(failure_line("test/b_test.exs", message), None);
        assert_eq!(failure_line("a_test.exs", "a_test.exs:x"), None);
        assert_eq!(
            failure_line("a_test.exs", "a_test.exs:0 a_test.exs:5"),
            Some(5)
        );
    }

    #[test]
    fn annotates_errors_and_warnings() {
        let mut report = collect(&sample());
        report.entries[1].label = Label::Broken {
            since_sha: Some("beef".into()),
        };
        sort(&mut report);
        let lines = annotations(&report, true);
        assert_eq!(
            lines,
            [
                "::error file=services/platform/test/fails_test.exs,line=42,title=failed%3A test fails::boom%0Aat fails_test.exs:42",
                "::error file=services/platform/test/still_test.exs,title=failed%3A test still::second",
                "::error file=services/platform/test/errors_test.exs,title=broken%3A test errors::broken since beef%0Acrashed",
                "::warning file=services/platform/test/flaky_test.exs,title=flaky on retry%3A test flaky::test flaky failed then passed on retry",
            ]
        );
        assert_eq!(annotations(&report, false).len(), 3);
    }

    #[test]
    fn annotates_a_test_without_a_file() {
        let report = Report {
            repo: None,
            entries: vec![Entry {
                test_id: "x".into(),
                name: "t".into(),
                target: "//:x".into(),
                file: None,
                message: None,
                label: Label::New,
            }],
        };
        assert_eq!(
            annotations(&report, true),
            ["::error title=new%3A t::t failed"]
        );
    }

    #[test]
    fn renders_markdown_with_links_and_escaping() {
        let mut report = collect(&sample());
        report.entries[0].name = "a|b <c> `d`".into();
        report.entries[0].label = Label::New;
        sort(&mut report);
        let md = markdown(&report, Some("https://srv.example/"), None);
        assert!(md.starts_with("## Aster test results\n\n**3 failing tests** · 1 new · 2 unlabelled · 1 flaky on retry\n"));
        assert!(
            md.contains(
                "| new | [a\\|b \\<c\\> \\`d\\`](https://srv.example/ui/tests/fails?repo=a%2Fb) \
                 | <code>test/fails_test.exs</code> | <code>//services/platform:test-ci</code> | boom |"
            ),
            "{md}"
        );
        assert!(md.contains("| flaky on retry |"));
        // Without a URL: plain names, and a note on missing labels.
        let plain = markdown(&report, None, Some("503"));
        assert!(plain.contains("| new | a\\|b"));
        assert!(plain.contains("not labelled new, broken or known flake: 503"));
        assert!(markdown(&collect(&[run_record()]), None, None).contains("No failing tests."));
    }

    fn results_file(records: &[Record]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}", aster_results_model::to_ndjson(records)).unwrap();
        file
    }

    fn env_of(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn labels_against_a_server_and_writes_the_step_summary() {
        let (url, seen) = serve(vec![
            (
                "/v1/test-results/tests/fails",
                200,
                r#"{"test":{"state":"flaky"}}"#.into(),
            ),
            (
                "/v1/test-results/tests/errors",
                200,
                r#"{"test":{"state":"broken","broken_since_sha":"abcdef0123456789"}}"#.into(),
            ),
            // `still` is unknown (404 from the fallthrough).
        ]);
        let file = results_file(&sample());
        let step = tempfile::NamedTempFile::new().unwrap();
        let env = env_of(vec![
            ("ASTER_SRV_TOKEN", "t".into()),
            ("GITHUB_STEP_SUMMARY", step.path().display().to_string()),
        ]);
        let mut out = Vec::new();
        run(
            &Options {
                path: file.path(),
                url: Some(url.clone()),
                github: true,
                flaky_warnings: true,
            },
            &env,
            &mut out,
        )
        .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 3);
        let summary = std::fs::read_to_string(step.path()).unwrap();
        let rows: Vec<&str> = summary
            .lines()
            .filter(|l| l.starts_with("| ") && !l.starts_with("| |"))
            .map(|l| l.split(" | ").next().unwrap())
            .collect();
        assert_eq!(
            rows,
            [
                "| new",
                "| broken since abcdef0123",
                "| known flake",
                "| flaky on retry"
            ]
        );
        assert!(summary.contains(&format!("{url}/ui/tests/still?repo=a%2Fb")));
        let out = String::from_utf8(out).unwrap();
        let titles: Vec<&str> = out
            .lines()
            .map(|l| {
                l.split("title=")
                    .nth(1)
                    .unwrap()
                    .split("::")
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            titles,
            [
                "new%3A test still",
                "broken%3A test errors",
                "known flake%3A test fails",
                "flaky on retry%3A test flaky"
            ]
        );
    }

    #[test]
    fn an_unreachable_server_leaves_failures_unlabelled() {
        let file = results_file(&sample());
        let env = env_of(vec![("ASTER_SRV_TOKEN", "t".into())]);
        let mut out = Vec::new();
        run(
            &Options {
                path: file.path(),
                url: Some(closed_port()),
                github: false,
                flaky_warnings: true,
            },
            &env,
            &mut out,
        )
        .unwrap();
        let md = String::from_utf8(out).unwrap();
        assert!(
            md.contains("**3 failing tests** · 1 flaky on retry"),
            "{md}"
        );
        assert!(md.contains("| failed |"));
        assert!(md.contains("cannot reach"), "{md}");
    }

    #[test]
    fn works_without_a_url_or_credentials() {
        let file = results_file(&sample());
        let mut out = Vec::new();
        for url in [None, Some("http://localhost:1".to_string())] {
            out.clear();
            run(
                &Options {
                    path: file.path(),
                    url,
                    github: false,
                    flaky_warnings: true,
                },
                &env_of(vec![]),
                &mut out,
            )
            .unwrap();
            let md = String::from_utf8(out.clone()).unwrap();
            assert!(md.contains("| failed | "), "{md}");
            assert!(md.contains("test fails"), "{md}");
        }
    }

    #[test]
    fn a_clean_run_writes_nothing_in_github_mode() {
        let file = results_file(&[run_record(), test("ok", TestOutcome::Pass, 1, None)]);
        let step = tempfile::NamedTempFile::new().unwrap();
        let env = env_of(vec![(
            "GITHUB_STEP_SUMMARY",
            step.path().display().to_string(),
        )]);
        let mut out = Vec::new();
        run(
            &Options {
                path: file.path(),
                url: None,
                github: true,
                flaky_warnings: true,
            },
            &env,
            &mut out,
        )
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(std::fs::read_to_string(step.path()).unwrap(), "");
    }

    #[test]
    fn automatic_summaries_leave_out_flaky_warnings() {
        let file = results_file(&sample());
        let mut out = Vec::new();
        run(
            &Options {
                path: file.path(),
                url: None,
                github: true,
                flaky_warnings: false,
            },
            &env_of(vec![]),
            &mut out,
        )
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.matches("::error").count(), 3);
        assert!(!out.contains("::warning"));
    }
}
