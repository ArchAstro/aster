//! Quarantine: letting a target pass when its only failures are tests known
//! to be flaky.
//!
//! The list has two sources:
//!
//! - aster-srv's (`GET /v1/test-results/quarantine`), fetched once when a
//!   run starts and cached in `.aster/quarantine.json`. When the server
//!   cannot be reached, a cached list younger than 24 hours stands in;
//!   otherwise there is none.
//! - `aster-quarantine.toml` at the workspace root, reviewed like any other
//!   change. Each entry needs a `reason` and an `expires` date and is
//!   ignored once that date passes.
//!
//! A failed target reports passed, with the tests it let through, only when
//! all of these hold ([`Quarantine::judge`]):
//!
//! 1. its captured results are `complete`;
//! 2. after retries, every test whose last attempt failed is quarantined,
//!    and there is at least one;
//! 3. it exited with its runner's "tests failed" code, not a crash, a
//!    compile error, a timeout or a signal;
//! 4. its captured console log does not show a non-test failure (a coverage
//!    threshold or an unhandled error; see `NON_TEST_FAILURE_MARKERS`). A
//!    log cut at the capture cap can drop that line; Aster does not claim to
//!    see past the cap;
//! 5. `ASTER_QUARANTINE` is not `off`.
//!
//! Quarantined tests still run and their results are still uploaded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::client::Server;
use super::upload::{proxy_token_from_env, Credentials};
use super::{same_test, TestCase, TestReport};
use crate::test_results::Confidence;

/// The repository's own list, at the workspace root.
pub const REPO_FILE: &str = "aster-quarantine.toml";

/// Substrings of a runner console line that mean the process failed for a
/// reason other than a test case. Matched case-insensitively; the whole
/// line is what [`Quarantine::judge`] names.
///
/// The exit code cannot tell these apart from a failed test: Jest, pytest
/// and Vitest all exit 1. The phrases are the runners' own messages:
///
/// - Jest: `Jest: "global" coverage threshold for lines (100%) not met`
/// - pytest-cov: `FAIL Required test coverage of N% not reached`
/// - Vitest: `ERROR: Coverage for lines (..%) does not meet global threshold`
/// - Vitest: `Vitest caught N unhandled error` and `Unhandled error`
const NON_TEST_FAILURE_MARKERS: &[&str] = &[
    "coverage threshold for ",
    "fail required test coverage of ",
    "does not meet global threshold",
    "unhandled error",
];

/// How long a cached server list may stand in for the server.
const CACHE_HOURS: i64 = 24;

/// How long the start of a run waits for the server's list.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One entry of [`REPO_FILE`]. It names a test by `id` (as the dashboard
/// shows it), or by `name` with any of `file`, `suite` and `target` to
/// narrow it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub suite: Option<String>,
    /// A project (`//services/platform`) or one of its targets
    /// (`//services/platform:test`).
    #[serde(default)]
    pub target: Option<String>,
    pub reason: String,
    /// The last day the entry applies, a TOML date such as `2026-11-30`.
    #[serde(deserialize_with = "toml_date")]
    pub expires: NaiveDate,
}

fn toml_date<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<NaiveDate, D::Error> {
    let value = toml::value::Datetime::deserialize(deserializer)?;
    match (value.date, value.time) {
        (Some(date), None) => NaiveDate::from_ymd_opt(
            i32::from(date.year),
            u32::from(date.month),
            u32::from(date.day),
        )
        .ok_or_else(|| serde::de::Error::custom("not a calendar date")),
        _ => Err(serde::de::Error::custom(
            "expires must be a date such as 2026-11-30",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoFile {
    #[serde(default)]
    test: Vec<RepoEntry>,
}

impl RepoEntry {
    fn matches(&self, address: &str, test_id: &str, case: &TestCase) -> bool {
        if let Some(id) = &self.id {
            return id == test_id;
        }
        let project = address.rsplit_once(':').map_or(address, |(p, _)| p);
        self.name.as_deref() == Some(case.name.as_str())
            && self
                .file
                .as_ref()
                .is_none_or(|file| case.file.as_deref() == Some(file.as_str()))
            && self
                .suite
                .as_ref()
                .is_none_or(|suite| case.suite.as_deref() == Some(suite.as_str()))
            && self
                .target
                .as_ref()
                .is_none_or(|target| target == address || target == project)
    }

    fn describe(&self) -> String {
        self.id
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_default()
    }
}

/// Read [`REPO_FILE`] from `root`: its entries still in force on `today`,
/// and a warning for each that has expired. No file is no entries.
pub fn read_repo_file(root: &Path, today: NaiveDate) -> Result<(Vec<RepoEntry>, Vec<String>)> {
    let path = root.join(REPO_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()))
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read {REPO_FILE}")),
    };
    let file: RepoFile = toml::from_str(&text).with_context(|| {
        format!(
            "{REPO_FILE} must hold [[test]] entries with a reason, an expires date, \
             and an id or a name"
        )
    })?;
    let mut live = Vec::new();
    let mut warnings = Vec::new();
    for (index, entry) in file.test.into_iter().enumerate() {
        let number = index + 1;
        if entry.reason.trim().is_empty() {
            bail!("{REPO_FILE}: test {number} needs a reason");
        }
        if entry.id.is_some() == entry.name.is_some() {
            bail!("{REPO_FILE}: test {number} needs either an id or a name, not both");
        }
        if entry.id.is_some()
            && (entry.file.is_some() || entry.suite.is_some() || entry.target.is_some())
        {
            bail!("{REPO_FILE}: test {number} names an id, so file, suite and target say nothing");
        }
        if entry.expires < today {
            warnings.push(format!(
                "{REPO_FILE}: the entry for {} expired on {}; it no longer applies",
                entry.describe(),
                entry.expires
            ));
            continue;
        }
        live.push(entry);
    }
    Ok((live, warnings))
}

/// The server's list as cached in `.aster/quarantine.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerList {
    /// The server it came from.
    pub url: String,
    pub repo: String,
    pub version: i64,
    #[serde(with = "rfc3339")]
    pub fetched_at: DateTime<Utc>,
    pub test_ids: Vec<String>,
}

mod rfc3339 {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(at: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&at.to_rfc3339())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&text)
            .map(|at| at.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

impl ServerList {
    /// From the body of `GET /v1/test-results/quarantine`.
    fn from_reply(url: &str, reply: &serde_json::Value, now: DateTime<Utc>) -> Result<Self> {
        let tests = reply["tests"]
            .as_array()
            .context("the quarantine list has no tests")?;
        Ok(Self {
            url: url.to_string(),
            repo: reply["repo"].as_str().unwrap_or_default().to_string(),
            version: reply["version"]
                .as_i64()
                .context("the quarantine list has no version")?,
            fetched_at: now,
            test_ids: tests
                .iter()
                .filter_map(|test| test["test_id"].as_str().map(str::to_string))
                .collect(),
        })
    }
}

fn cache_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".aster").join("quarantine.json")
}

/// Where the server list came from, for the line printed at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSource {
    /// Just fetched.
    Fetched,
    /// The server could not be reached (the reason); the cache stood in.
    Cached(String),
}

/// The tests a run lets fail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quarantine {
    pub server: Option<(ServerList, ServerSource)>,
    server_ids: HashSet<String>,
    pub entries: Vec<RepoEntry>,
}

/// What [`Quarantine::judge`] decided for a failed target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every failure is quarantined: the target passes. The tests, with
    /// where their quarantine comes from.
    Pass(Vec<Quarantined>),
    /// Some failures are quarantined, but the target still fails, for the
    /// reason given.
    Fail(String),
    /// None of its failures is quarantined.
    Untouched,
}

/// A failing test the quarantine let through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Quarantined {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// `aster-srv`, or the repository file's reason.
    pub source: String,
}

impl Quarantine {
    pub fn new(server: Option<(ServerList, ServerSource)>, entries: Vec<RepoEntry>) -> Self {
        let server_ids = server
            .as_ref()
            .map(|(list, _)| list.test_ids.iter().cloned().collect())
            .unwrap_or_default();
        Self {
            server,
            server_ids,
            entries,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.server_ids.is_empty() && self.entries.is_empty()
    }

    /// Why `case` (of the target at `address`, recorded under `project`
    /// when its report names one) is quarantined, if it is.
    pub fn source_of(
        &self,
        address: &str,
        project: Option<&str>,
        case: &TestCase,
    ) -> Option<String> {
        let test_id = super::stream::test_id(address, project, case);
        if self.server_ids.contains(&test_id) {
            return Some("aster-srv".to_string());
        }
        self.entries
            .iter()
            .find(|entry| entry.matches(address, &test_id, case))
            .map(|entry| format!("{REPO_FILE}: {}", entry.reason))
    }

    /// Whether a failed target may pass. `plugin` names the target's
    /// project plugin, `command` is the target's one command as written
    /// (several commands are never judged), `exit_code` its last exit code,
    /// `report` its results after retries, and `output` the captured console
    /// log. A line in `output` that matches `NON_TEST_FAILURE_MARKERS`
    /// refuses a pass: the exit code cannot tell that line from a test failure.
    pub fn judge(
        &self,
        address: &str,
        plugin: &str,
        command: &str,
        exit_code: Option<i32>,
        report: &TestReport,
        output: &str,
    ) -> Verdict {
        let failing = last_failures(&report.cases);
        let mut quarantined = Vec::new();
        let mut unquarantined = 0;
        for case in &failing {
            match self.source_of(address, report.project.as_deref(), case) {
                Some(source) => quarantined.push(Quarantined {
                    name: case.name.clone(),
                    suite: case.suite.clone(),
                    file: case.file.clone(),
                    source,
                }),
                None => unquarantined += 1,
            }
        }
        if quarantined.is_empty() {
            return Verdict::Untouched;
        }
        let of = format!("{} of {} failing tests", quarantined.len(), failing.len());
        if unquarantined > 0 {
            return Verdict::Fail(format!("{of} are quarantined; the others still fail"));
        }
        if report.confidence != Confidence::Complete {
            return Verdict::Fail(format!(
                "{of} are quarantined, but the results are partial, so other failures may be hidden"
            ));
        }
        let codes = tests_failed_codes(plugin, &report.capture, command);
        match exit_code {
            Some(code) if codes.contains(&code) => match non_test_failure(output) {
                Some(line) => Verdict::Fail(format!(
                    "{of} are quarantined, but the runner also reported {line}, which is not a test failure"
                )),
                None => Verdict::Pass(quarantined),
            },
            _ if codes.is_empty() => Verdict::Fail(format!(
                "{of} are quarantined, but Aster cannot tell this runner's test failures from \
                 other errors by its exit code"
            )),
            code => Verdict::Fail(format!(
                "{of} are quarantined, but the command exited {}, not with the runner's \
                 \"tests failed\" code {}",
                code.map_or("by a signal or not at all".to_string(), |c| c.to_string()),
                codes
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
        }
    }

    /// The line printed when a run starts.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        match &self.server {
            Some((list, ServerSource::Fetched)) => parts.push(format!(
                "{} from aster-srv (version {})",
                list.test_ids.len(),
                list.version
            )),
            Some((list, ServerSource::Cached(why))) => parts.push(format!(
                "{} from aster-srv (version {}, cached {}; {why})",
                list.test_ids.len(),
                list.version,
                list.fetched_at.format("%Y-%m-%d %H:%M UTC"),
            )),
            None => {}
        }
        if !self.entries.is_empty() {
            parts.push(format!("{} from {REPO_FILE}", self.entries.len()));
        }
        if parts.is_empty() {
            "no tests".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// The first console line that reports a failure other than a test case.
///
/// Aster's own lines are not the runner: a `$ command` echo can contain a
/// retried test's name (`-t 'unhandled error$'`), and `[aster]` notes are ours.
fn non_test_failure(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("$ ") || trimmed.starts_with("[aster]") {
            return None;
        }
        let lower = trimmed.to_ascii_lowercase();
        NON_TEST_FAILURE_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
            .then_some(trimmed)
    })
}

/// The tests whose last attempt failed, once each.
fn last_failures(cases: &[TestCase]) -> Vec<&TestCase> {
    let mut failing: Vec<&TestCase> = Vec::new();
    for case in cases {
        let last = cases
            .iter()
            .filter(|other| same_test(other, case))
            .map(|other| other.attempt)
            .max();
        if case.outcome.is_failure()
            && last == Some(case.attempt)
            && !failing.iter().any(|known| same_test(known, case))
        {
            failing.push(case);
        }
    }
    failing
}

/// The exit codes with which a runner says "tests failed, nothing else
/// went wrong". Empty when a runner uses one code for both.
pub fn tests_failed_codes(plugin: &str, capture: &str, command: &str) -> &'static [i32] {
    // Gradle and Maven exit 1 for compile errors too, however the command
    // is spelled.
    if matches!(plugin, "gradle" | "maven") {
        return &[];
    }
    let programs: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || c == '\'' || c == '"' || c == ';' || c == '&')
        .map(|word| word.rsplit('/').next().unwrap_or(word))
        .collect();
    let runs = |name: &str| programs.contains(&name);
    // A declared report's command is a script unless it names its runner:
    // then the runner's own code applies, as for a report Aster planned.
    let (capture, declared) = match capture.strip_prefix("declared-") {
        Some(base) => (base, true),
        None => (capture, false),
    };
    if declared
        && (["sh", "bash", "zsh", "dash"]
            .iter()
            .any(|shell| runs(shell))
            || ![
                "vitest", "jest", "bun", "pytest", "go", "mix", "cargo", "nextest",
            ]
            .iter()
            .any(|runner| runs(runner)))
    {
        // The exit code of a script says nothing certain about its runner:
        // it may be a test failure or any other step.
        return &[];
    }
    match capture {
        // ExUnit exits 2 for failed tests and 1 for a compile error.
        "exunit" => &[2],
        // A build failure makes go-json results partial, so 1 is tests.
        "go-json" | "jest-json" | "node-test" => &[1],
        "junit" if runs("nextest") => &[100],
        "junit" if ["gradle", "gradlew", "mvn", "mvnw"].iter().any(|p| runs(p)) => &[],
        // pytest: 1 is failed tests, 2+ is interrupted or broken. vitest
        // and bun: 1.
        "junit" => &[1],
        // cargo test exits 101 for a failed test and for a compile error. A
        // compile error runs no test, so a complete report with a failing
        // test is a test failure.
        "libtest" => &[101],
        _ => &[],
    }
}

/// Load the quarantine for a run: `None` when `ASTER_QUARANTINE=off`, or
/// when there is no list at all. Prints what it found, and why the server's
/// list is missing when it is.
pub fn load(
    workspace_root: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Quarantine>> {
    if env("ASTER_QUARANTINE").is_some_and(|value| value.eq_ignore_ascii_case("off")) {
        eprintln!("[aster] Quarantine: off (ASTER_QUARANTINE=off)");
        return Ok(None);
    }
    let now = Utc::now();
    let (entries, warnings) = read_repo_file(workspace_root, now.date_naive())?;
    for warning in warnings {
        eprintln!("[aster] Warning: {warning}");
    }
    let server = env("ASTER_SRV_URL")
        .filter(|url| !url.is_empty())
        .and_then(|url| {
            let fetched = fetch(&url, env, now);
            server_list(workspace_root, &url, fetched, now)
        });
    let quarantine = Quarantine::new(server, entries);
    if quarantine.is_empty() && quarantine.server.is_none() {
        return Ok(None);
    }
    eprintln!("[aster] Quarantine: {}", quarantine.describe());
    Ok(Some(quarantine))
}

fn fetch(
    url: &str,
    env: &dyn Fn(&str) -> Option<String>,
    now: DateTime<Utc>,
) -> Result<ServerList> {
    let credentials = Credentials::from_env(env).context(
        "no credentials: set ASTER_SRV_TOKEN, or give the job `permissions: id-token: write`",
    )?;
    let server =
        Server::connect_with_timeout(url, &credentials, proxy_token_from_env(env), FETCH_TIMEOUT)?;
    let reply = server
        .get_json("/v1/test-results/quarantine")?
        .context("the server has no quarantine list (is it older than this Aster?)")?;
    ServerList::from_reply(url, &reply, now)
}

/// The server's list: `fetched` when it worked (and cached), else a cache
/// younger than [`CACHE_HOURS`] from the same server.
fn server_list(
    workspace_root: &Path,
    url: &str,
    fetched: Result<ServerList>,
    now: DateTime<Utc>,
) -> Option<(ServerList, ServerSource)> {
    let path = cache_path(workspace_root);
    match fetched {
        Ok(list) => {
            let written = std::fs::create_dir_all(path.parent().expect("a parent"))
                .map_err(anyhow::Error::from)
                .and_then(|()| Ok(serde_json::to_vec(&list)?))
                .and_then(|json| Ok(std::fs::write(&path, json)?));
            if let Err(error) = written {
                eprintln!("[aster] Warning: cannot cache the quarantine list: {error:#}");
            }
            Some((list, ServerSource::Fetched))
        }
        Err(error) => {
            let why = format!("{error:#}");
            let cached = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<ServerList>(&bytes).ok())
                .filter(|list| list.url == url)
                .filter(|list| now - list.fetched_at < chrono::Duration::hours(CACHE_HOURS));
            match cached {
                Some(list) => Some((list, ServerSource::Cached(why))),
                None => {
                    eprintln!(
                        "[aster] Quarantine: aster-srv's list is unavailable ({why}) and no \
                         list cached in the last {CACHE_HOURS} hours; only {REPO_FILE} applies"
                    );
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_results::TestOutcome;

    fn case(name: &str, outcome: TestOutcome, attempt: u32) -> TestCase {
        TestCase {
            file: Some("test/a_test.exs".into()),
            suite: Some("A".into()),
            name: name.into(),
            outcome,
            attempt,
            duration_ms: None,
            message: None,
        }
    }

    fn report(cases: Vec<TestCase>) -> TestReport {
        TestReport {
            project: None,
            capture: "exunit".into(),
            confidence: Confidence::Complete,
            cases,
            quarantined: Vec::new(),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
    }

    fn entry(name: &str) -> RepoEntry {
        RepoEntry {
            id: None,
            name: Some(name.into()),
            file: None,
            suite: None,
            target: None,
            reason: "flaky on CI".into(),
            expires: today(),
        }
    }

    const ADDRESS: &str = "//services/platform:test";
    const MIX: &str = "mix test";

    #[test]
    fn passes_only_when_every_last_failure_is_quarantined() {
        let q = Quarantine::new(None, vec![entry("flaky")]);
        let only_flaky = report(vec![
            case("flaky", TestOutcome::Fail, 1),
            case("fine", TestOutcome::Pass, 1),
        ]);
        let Verdict::Pass(tests) = q.judge(ADDRESS, "elixir", MIX, Some(2), &only_flaky, "") else {
            panic!("expected a pass");
        };
        assert_eq!(tests[0].name, "flaky");
        assert_eq!(tests[0].source, "aster-quarantine.toml: flaky on CI");

        let also_real = report(vec![
            case("flaky", TestOutcome::Fail, 1),
            case("real", TestOutcome::Error, 1),
        ]);
        assert!(matches!(
            q.judge(ADDRESS, "elixir", MIX, Some(2), &also_real, ""),
            Verdict::Fail(why) if why.contains("1 of 2")
        ));
        // A retry that fixed the other failure leaves only the quarantined one.
        let retried = report(vec![
            case("flaky", TestOutcome::Fail, 1),
            case("real", TestOutcome::Fail, 1),
            case("flaky", TestOutcome::Fail, 2),
            case("real", TestOutcome::Pass, 2),
        ]);
        assert!(matches!(
            q.judge(ADDRESS, "elixir", MIX, Some(2), &retried, ""),
            Verdict::Pass(tests) if tests.len() == 1
        ));
        let unrelated = report(vec![case("real", TestOutcome::Fail, 1)]);
        assert_eq!(
            q.judge(ADDRESS, "elixir", MIX, Some(2), &unrelated, ""),
            Verdict::Untouched
        );
    }

    #[test]
    fn never_passes_partial_results_or_other_exit_codes() {
        let q = Quarantine::new(None, vec![entry("flaky")]);
        let mut partial = report(vec![case("flaky", TestOutcome::Fail, 1)]);
        partial.confidence = Confidence::Partial;
        assert!(matches!(
            q.judge(ADDRESS, "elixir", MIX, Some(2), &partial, ""),
            Verdict::Fail(why) if why.contains("partial")
        ));
        let failed = report(vec![case("flaky", TestOutcome::Fail, 1)]);
        // A compile error (1), a signal (None) or a timeout are not test failures.
        for code in [Some(1), Some(137), None] {
            assert!(
                matches!(
                    q.judge(ADDRESS, "elixir", MIX, code, &failed, ""),
                    Verdict::Fail(_)
                ),
                "{code:?}"
            );
        }
        let mut cargo = failed.clone();
        cargo.capture = "libtest".into();
        // cargo test: 101 for failed tests; a different code is not.
        assert!(matches!(
            q.judge(ADDRESS, "rust", "cargo test", Some(1), &cargo, ""),
            Verdict::Fail(why) if why.contains("101")
        ));
    }

    #[test]
    fn a_non_test_failure_in_the_console_is_not_excused() {
        let q = Quarantine::new(None, vec![entry("flaky")]);
        let mut failed = report(vec![
            case("flaky", TestOutcome::Fail, 1),
            case("fine", TestOutcome::Pass, 1),
        ]);
        failed.capture = "jest-json".into();
        let markers = [
            r#"Jest: "global" coverage threshold for lines (100%) not met: 80%"#,
            "FAIL Required test coverage of 80% not reached. Total coverage: 70.00%",
            "ERROR: Coverage for lines (70%) does not meet global threshold (100%)",
            "Vitest caught 1 unhandled error during the test run",
            "Unhandled error",
        ];
        for marker in markers {
            let output = format!("noise\n{marker}\nmore\n");
            match q.judge(
                ADDRESS,
                "nodejs",
                "jest --coverage",
                Some(1),
                &failed,
                &output,
            ) {
                Verdict::Fail(why) => {
                    assert!(why.contains(marker), "{why}");
                    assert!(why.contains("not a test failure"), "{why}");
                    assert!(why.contains("the runner also reported"), "{why}");
                }
                other => panic!("expected fail for {marker}, got {other:?}"),
            }
        }
        // The same report with no marker still passes.
        assert!(matches!(
            q.judge(
                ADDRESS,
                "nodejs",
                "jest --coverage",
                Some(1),
                &failed,
                "Tests: 1 failed, 1 passed\n"
            ),
            Verdict::Pass(tests) if tests.len() == 1
        ));
        // A truncated log whose marker was dropped is not claimed as seen.
        // Aster's command echo and its own notes are not the runner.
        assert!(non_test_failure("[aster] log truncated: 12 bytes omitted\n").is_none());
        assert!(non_test_failure("$ jest --coverage -t 'unhandled error$'\n").is_none());
        assert_eq!(
            non_test_failure("$ jest -t 'unhandled error$'\nVitest caught 1 unhandled error\n"),
            Some("Vitest caught 1 unhandled error")
        );
        // Partial results and a real failure keep their own reasons.
        let mut partial = failed.clone();
        partial.confidence = Confidence::Partial;
        let marked = r#"Jest: "global" coverage threshold for lines (100%) not met"#;
        assert!(matches!(
            q.judge(ADDRESS, "nodejs", "jest --coverage", Some(1), &partial, marked),
            Verdict::Fail(why) if why.contains("partial") && !why.contains("not a test failure")
        ));
        let also_real = report(vec![
            case("flaky", TestOutcome::Fail, 1),
            case("real", TestOutcome::Fail, 1),
        ]);
        assert!(matches!(
            q.judge(ADDRESS, "nodejs", "jest --coverage", Some(1), &also_real, marked),
            Verdict::Fail(why) if why.contains("the others still fail")
        ));
        let unrelated = report(vec![case("real", TestOutcome::Fail, 1)]);
        assert_eq!(
            q.judge(
                ADDRESS,
                "nodejs",
                "jest --coverage",
                Some(1),
                &unrelated,
                marked
            ),
            Verdict::Untouched
        );
    }

    #[test]
    fn knows_each_runners_tests_failed_code() {
        assert_eq!(tests_failed_codes("elixir", "exunit", "mix test"), &[2]);
        assert_eq!(
            tests_failed_codes(
                "elixir",
                "exunit",
                "bash -c 'cd ../platform && MIX_TEST_PARTITION=1 exec mix test'"
            ),
            &[2]
        );
        let none: &[i32] = &[];
        assert_eq!(
            tests_failed_codes("rust", "junit", "cargo nextest run"),
            &[100]
        );
        assert_eq!(tests_failed_codes("python", "junit", "pytest -q"), &[1]);
        assert_eq!(tests_failed_codes("java", "junit", "./gradlew test"), none);
        assert_eq!(
            tests_failed_codes("gradle", "junit", "sh -c 'make test'"),
            none
        );
        assert_eq!(tests_failed_codes("maven", "junit", "mvn -q test"), none);
        assert_eq!(tests_failed_codes("go", "go-json", "go test ./..."), &[1]);
        assert_eq!(tests_failed_codes("rust", "libtest", "cargo test"), &[101]);
        // A script's exit code says nothing certain about its runner.
        assert_eq!(
            tests_failed_codes("go", "declared-junit", "bash tools/test.sh"),
            none
        );
        assert_eq!(
            tests_failed_codes("go", "declared-go-json", "sh -c 'go test ./... > r.json'"),
            none
        );
    }

    #[test]
    fn server_entries_match_by_test_id() {
        let failing = case("flaky", TestOutcome::Fail, 1);
        let id = crate::test_results::stream::test_id(ADDRESS, None, &failing);
        let list = ServerList {
            url: "https://srv".into(),
            repo: "o/r".into(),
            version: 7,
            fetched_at: Utc::now(),
            test_ids: vec![id],
        };
        let q = Quarantine::new(Some((list, ServerSource::Fetched)), Vec::new());
        assert_eq!(
            q.source_of(ADDRESS, None, &failing).as_deref(),
            Some("aster-srv")
        );
        // The same test under another project is another test.
        assert_eq!(q.source_of("//services/other:test", None, &failing), None);
        // A shard running another project's suite records it under that
        // project, so the list matches it there.
        let shard = crate::test_results::stream::test_id(
            "//services/shard:test",
            Some("//services/platform"),
            &failing,
        );
        assert_eq!(
            shard,
            crate::test_results::stream::test_id("//services/platform:test", None, &failing)
        );
        assert_eq!(
            q.source_of(
                "//services/shard:test",
                Some("//services/platform"),
                &failing
            )
            .as_deref(),
            Some("aster-srv")
        );
        assert_eq!(q.describe(), "1 from aster-srv (version 7)");
    }

    #[test]
    fn repo_entries_narrow_by_file_suite_and_target() {
        let failing = case("flaky", TestOutcome::Fail, 1);
        let id = crate::test_results::stream::test_id(ADDRESS, None, &failing);
        let by = |change: &dyn Fn(&mut RepoEntry)| {
            let mut e = entry("flaky");
            change(&mut e);
            e.matches(ADDRESS, &id, &failing)
        };
        assert!(by(&|_| {}));
        assert!(by(&|e| e.file = Some("test/a_test.exs".into())));
        assert!(!by(&|e| e.file = Some("test/b_test.exs".into())));
        assert!(by(&|e| e.suite = Some("A".into())));
        assert!(!by(&|e| e.suite = Some("B".into())));
        assert!(by(&|e| e.target = Some("//services/platform".into())));
        assert!(by(&|e| e.target = Some(ADDRESS.into())));
        assert!(!by(&|e| e.target = Some("//services/plat".into())));
        assert!(by(&|e| {
            e.name = None;
            e.id = Some(id.clone());
        }));
    }

    #[test]
    fn reads_the_repo_file_and_drops_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_repo_file(dir.path(), today()).unwrap().0, Vec::new());
        std::fs::write(
            dir.path().join(REPO_FILE),
            r#"
[[test]]
name = "creates an agent"
file = "test/platform/agents_test.exs"
reason = "DBConnection.OwnershipError, #123"
expires = 2026-10-15

[[test]]
id = "abc"
reason = "old"
expires = 2026-10-14
"#,
        )
        .unwrap();
        let (live, warnings) = read_repo_file(dir.path(), today()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].name.as_deref(), Some("creates an agent"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("abc") && warnings[0].contains("2026-10-14"));

        for bad in [
            "[[test]]\nname = \"x\"\nexpires = 2026-11-01\n",
            "[[test]]\nname = \"x\"\nreason = \"r\"\n",
            "[[test]]\nname = \"x\"\nreason = \" \"\nexpires = 2026-11-01\n",
            "[[test]]\nreason = \"r\"\nexpires = 2026-11-01\n",
            "[[test]]\nid = \"a\"\nname = \"x\"\nreason = \"r\"\nexpires = 2026-11-01\n",
            "[[test]]\nid = \"a\"\nfile = \"f\"\nreason = \"r\"\nexpires = 2026-11-01\n",
            "[[test]]\nname = \"x\"\nreason = \"r\"\nexpires = 2026-11-01\nowner = \"me\"\n",
        ] {
            std::fs::write(dir.path().join(REPO_FILE), bad).unwrap();
            assert!(read_repo_file(dir.path(), today()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_fresh_cache_stands_in_for_an_unreachable_server() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let list = ServerList {
            url: "https://srv".into(),
            repo: "o/r".into(),
            version: 3,
            fetched_at: now,
            test_ids: vec!["t".into()],
        };
        let fetched = server_list(dir.path(), "https://srv", Ok(list.clone()), now);
        assert_eq!(fetched, Some((list.clone(), ServerSource::Fetched)));

        let down = || Err(anyhow::anyhow!("connection refused"));
        let later = now + chrono::Duration::hours(23);
        let Some((cached, ServerSource::Cached(why))) =
            server_list(dir.path(), "https://srv", down(), later)
        else {
            panic!("expected the cache");
        };
        assert_eq!(cached, list);
        assert!(why.contains("refused"));
        // Too old, or from another server: nothing.
        let stale = now + chrono::Duration::hours(25);
        assert_eq!(server_list(dir.path(), "https://srv", down(), stale), None);
        assert_eq!(
            server_list(dir.path(), "https://other", down(), later),
            None
        );
    }

    #[test]
    fn reads_the_servers_reply() {
        let reply = serde_json::json!({
            "repo": "o/r", "version": 12, "generated_at": "x", "truncated": false,
            "tests": [{"test_id": "a"}, {"test_id": "b"}],
        });
        let list = ServerList::from_reply("https://srv", &reply, Utc::now()).unwrap();
        assert_eq!(list.version, 12);
        assert_eq!(list.test_ids, vec!["a", "b"]);
        assert!(ServerList::from_reply("u", &serde_json::json!({}), Utc::now()).is_err());
    }

    #[test]
    fn loads_the_servers_list_then_falls_back_to_its_cache() {
        use crate::test_results::client::testing::{closed_port, serve};
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(vec![(
            "/v1/test-results/quarantine",
            200,
            r#"{"repo":"o/r","version":9,"tests":[{"test_id":"a"}],"truncated":false}"#.into(),
        )]);
        let env = |url: String| {
            move |name: &str| match name {
                "ASTER_SRV_URL" => Some(url.clone()),
                "ASTER_SRV_TOKEN" => Some("t0ken".into()),
                _ => None,
            }
        };
        let loaded = load(dir.path(), &env(url.clone())).unwrap().unwrap();
        assert_eq!(loaded.describe(), "1 from aster-srv (version 9)");
        assert!(seen.lock().unwrap()[0].contains("authorization: bearer t0ken"));
        assert!(cache_path(dir.path()).exists());

        // The same server, down: the cache stands in. Another server: no list.
        std::fs::write(
            cache_path(dir.path()),
            std::fs::read_to_string(cache_path(dir.path()))
                .unwrap()
                .replace(&url, "http://down"),
        )
        .unwrap();
        let down = closed_port();
        assert_eq!(load(dir.path(), &env(down)).unwrap(), None);
        let cached = server_list(
            dir.path(),
            "http://down",
            Err(anyhow::anyhow!("refused")),
            Utc::now(),
        );
        assert!(matches!(cached, Some((_, ServerSource::Cached(_)))));
    }

    /// The ```toml blocks of a Markdown file that hold `[[test]]` entries.
    fn toml_examples(markdown: &str) -> Vec<String> {
        markdown
            .split("```toml\n")
            .skip(1)
            .filter_map(|block| block.split("```").next())
            .filter(|block| block.contains("[[test]]"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_documented_quarantine_files_work() {
        let docs = [
            include_str!("../../docs/test-tracking.md"),
            include_str!("../../README.md"),
        ];
        let examples: Vec<String> = docs.iter().flat_map(|d| toml_examples(d)).collect();
        assert_eq!(examples.len(), 2, "both documents show an example");
        let agents = TestCase {
            file: Some("test/platform/agents_test.exs".into()),
            suite: Some("Platform.AgentsTest".into()),
            name: "test creates an agent".into(),
            outcome: TestOutcome::Fail,
            attempt: 1,
            duration_ms: None,
            message: None,
        };
        for example in examples {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(REPO_FILE), &example).unwrap();
            let (entries, warnings) = read_repo_file(dir.path(), today()).unwrap();
            assert!(warnings.is_empty(), "{example}");
            let q = Quarantine::new(None, entries);
            assert!(
                q.source_of("//services/platform:test-ci", None, &agents)
                    .is_some(),
                "{example}"
            );
            if example.contains("target =") {
                assert!(q
                    .source_of("//services/other:test-ci", None, &agents)
                    .is_none());
            }
        }
    }

    #[test]
    fn off_switch_and_no_lists_load_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let off = |name: &str| (name == "ASTER_QUARANTINE").then(|| "off".to_string());
        std::fs::write(
            dir.path().join(REPO_FILE),
            "[[test]]\nname = \"x\"\nreason = \"r\"\nexpires = 2999-01-01\n",
        )
        .unwrap();
        assert_eq!(load(dir.path(), &off).unwrap(), None);
        let nothing = |_: &str| None;
        assert_eq!(
            load(dir.path(), &nothing).unwrap().unwrap().entries.len(),
            1
        );
        std::fs::remove_file(dir.path().join(REPO_FILE)).unwrap();
        assert_eq!(load(dir.path(), &nothing).unwrap(), None);
    }
}
