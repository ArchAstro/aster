//! The classifier against a model of its rules, on random histories.
//!
//! Each case seeds a few days of runs (branches, reused input hashes,
//! retries, skips, cached and partial targets, failure streaks, runs where
//! every test fails at once), classifies
//! them in passes at random times, and compares every flake event and test
//! state with what [`Model`] derives from the whole history at once. The
//! incremental SQL must agree with the rules however the passes fall.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use aster_srv::test_results::classifier::{
    self, INCIDENT_FAILURES, INCIDENT_SHARED_HASH, INCIDENT_SHARE_MIN,
};
use chrono::{DateTime, Duration, Utc};
use common::{seed, start, Run, Server};

/// A small deterministic generator (xorshift64*), so a failing case can be
/// replayed from its seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const TESTS: &[&str] = &["steady", "flips", "stuck", "rare", "skippy"];
const BROKEN_RUNS: usize = 3;

/// One attempt: `(test, outcome, attempt, failure_hash)`.
type Attempt = (&'static str, &'static str, i32, Option<&'static str>);

#[derive(Clone)]
struct GenRun {
    run_id: String,
    branch: &'static str,
    at: DateTime<Utc>,
    input_hash: &'static str,
    cached: bool,
    partial: bool,
    attempts: Vec<Attempt>,
}

fn outcome(rng: &mut Rng, test: &str) -> (&'static str, Option<&'static str>) {
    let fail_percent = match test {
        "steady" => 5,
        "stuck" => 88,
        "rare" => 15,
        _ => 40,
    };
    if rng.chance(fail_percent) {
        // Mostly one failure, sometimes another or none.
        let hash = match rng.below(10) {
            0 => None,
            1 | 2 => Some("f2"),
            _ => Some("f1"),
        };
        (if rng.chance(85) { "fail" } else { "error" }, hash)
    } else {
        ("pass", None)
    }
}

/// `runs` runs over a few days before `end`, in received order.
fn generate(seed: u64, runs: usize, end: DateTime<Utc>) -> Vec<GenRun> {
    let mut rng = Rng::new(seed);
    // About 70 runs at most 90 minutes apart fit in the four days before
    // `end`: the last pass at `end` sees them all.
    let mut at = end - Duration::days(4);
    let mut out = Vec::new();
    for i in 0..runs {
        at += Duration::minutes(1 + rng.below(90) as i64);
        // Two runs can share a received_at; run_id breaks the tie.
        if rng.chance(5) && !out.is_empty() {
            at = out.last().map(|r: &GenRun| r.at).unwrap();
        }
        let mut attempts = Vec::new();
        // Now and then a run where everything fails for one cause.
        let mass = rng.chance(6);
        for test in TESTS {
            if mass {
                attempts.push((*test, "error", 1, Some("boom")));
                continue;
            }
            if *test == "skippy" && rng.chance(40) {
                attempts.push((*test, "skip", 1, None));
                continue;
            }
            if rng.chance(8) {
                continue; // not run at all
            }
            let (first, hash) = outcome(&mut rng, test);
            attempts.push((*test, first, 1, hash));
            if first != "pass" && rng.chance(35) {
                let (second, hash) = outcome(&mut rng, test);
                attempts.push((*test, second, 2, hash));
                if rng.chance(10) {
                    attempts.push((*test, "skip", 3, None));
                }
            }
        }
        assert!(at < end, "runs must fall before the last pass");
        out.push(GenRun {
            run_id: format!("r{i:03}"),
            branch: rng.pick(&["main", "main", "main", "refs/heads/main", "pr-1", "pr-2"]),
            at,
            input_hash: rng.pick(&["h1", "h2", "h3", "h4", "h5", "h6"]),
            cached: rng.chance(8),
            partial: rng.chance(10),
            attempts,
        });
    }
    out
}

/// A test's final result in one run.
#[derive(Clone, Debug)]
struct Final {
    key: (DateTime<Utc>, String),
    sha: String,
    is_fail: bool,
    failure_hash: Option<String>,
    input_hash: String,
    on_default: bool,
    partial: bool,
    retried: bool,
}

/// Whether a run's target is an incident: its tests failed together.
fn incident(run: &GenRun) -> bool {
    if run.cached {
        return false;
    }
    let mut tests = 0i64;
    let mut failures = 0i64;
    let mut shared: BTreeMap<&str, i64> = BTreeMap::new();
    for test in TESTS {
        let tried: Vec<&Attempt> = run
            .attempts
            .iter()
            .filter(|a| a.0 == *test && a.1 != "skip")
            .collect();
        let Some(last) = tried.iter().max_by_key(|a| a.2) else {
            continue;
        };
        tests += 1;
        if tried.iter().any(|a| a.1 != "pass") {
            failures += 1;
        }
        if let (true, Some(hash)) = (last.1 != "pass", last.3) {
            *shared.entry(hash).or_default() += 1;
        }
    }
    let most = shared.values().copied().max().unwrap_or(0);
    failures >= INCIDENT_FAILURES
        || (failures >= INCIDENT_SHARE_MIN && failures * 2 >= tests)
        || most >= INCIDENT_SHARED_HASH
}

/// The rules (see the classifier's module docs), applied to the whole
/// history at once.
struct Model {
    /// `(test, kind, reason, run)`.
    events: BTreeSet<(String, String, String, String)>,
    /// test → (state, score, since_sha, failure_hash).
    states: BTreeMap<String, (String, i32, Option<String>, Option<String>)>,
    /// Runs whose target is an incident.
    incidents: BTreeSet<String>,
}

impl Model {
    fn new(runs: &[GenRun]) -> Self {
        let incidents: BTreeSet<String> = runs
            .iter()
            .filter(|r| incident(r))
            .map(|r| r.run_id.clone())
            .collect();
        let mut finals: BTreeMap<&str, Vec<Final>> = BTreeMap::new();
        for run in runs.iter().filter(|r| !r.cached) {
            for test in TESTS {
                let tried: Vec<&Attempt> = run
                    .attempts
                    .iter()
                    .filter(|a| a.0 == *test && a.1 != "skip")
                    .collect();
                let Some(last) = tried.iter().max_by_key(|a| a.2) else {
                    continue;
                };
                let is_fail = last.1 != "pass";
                finals.entry(test).or_default().push(Final {
                    key: (run.at, run.run_id.clone()),
                    sha: run.run_id.clone(),
                    is_fail,
                    failure_hash: last.3.map(str::to_string),
                    input_hash: run.input_hash.to_string(),
                    on_default: matches!(run.branch, "main" | "refs/heads/main"),
                    partial: run.partial,
                    retried: !is_fail && tried.iter().any(|a| a.1 != "pass"),
                });
            }
        }
        let mut events = BTreeSet::new();
        // Every test seen gets a row, healthy until shown otherwise.
        let mut states: BTreeMap<String, (String, i32, Option<String>, Option<String>)> = runs
            .iter()
            .flat_map(|r| r.attempts.iter().map(|a| a.0.to_string()))
            .map(|test| (test, ("healthy".to_string(), 0, None, None)))
            .collect();
        for (test, list) in &mut finals {
            list.sort_by(|a, b| a.key.cmp(&b.key));
            let event = |kind: &str, reason: &str, f: &Final| {
                (
                    test.to_string(),
                    kind.to_string(),
                    reason.to_string(),
                    f.key.1.clone(),
                )
            };
            // Every event is the failed run's, and an incident's failures
            // make none.
            let mut definite = BTreeSet::new();
            for (i, f) in list.iter().enumerate() {
                if f.retried && !incidents.contains(&f.key.1) {
                    events.insert(event("definite", "retry", f));
                    definite.insert(f.key.1.clone());
                }
                let earlier = list[..i]
                    .iter()
                    .rev()
                    .find(|p| p.input_hash == f.input_hash);
                if let Some(p) = earlier.filter(|p| p.is_fail != f.is_fail) {
                    let failed = if f.is_fail { f } else { p };
                    if !incidents.contains(&failed.key.1) {
                        events.insert(event("definite", "input_hash", failed));
                        definite.insert(failed.key.1.clone());
                    }
                }
            }
            // The default-branch, not-partial sequence decides likely
            // events and broken streaks.
            let mains: Vec<&Final> = list.iter().filter(|f| f.on_default && !f.partial).collect();
            let streak_at = |end: usize| -> (usize, Option<&str>) {
                let hash = mains[end].failure_hash.as_deref();
                let mut length = 0;
                let mut since = None;
                for f in mains[..=end].iter().rev() {
                    if f.is_fail && f.failure_hash.is_some() && f.failure_hash.as_deref() == hash {
                        length += 1;
                        since = Some(f.sha.as_str());
                    } else {
                        break;
                    }
                }
                (length, since)
            };
            for (i, f) in mains.iter().enumerate().skip(1) {
                let p = mains[i - 1];
                if !f.is_fail
                    && p.is_fail
                    && streak_at(i - 1).0 < BROKEN_RUNS
                    && !definite.contains(&p.key.1)
                    && !incidents.contains(&p.key.1)
                {
                    events.insert(event("likely", "next_main_run", p));
                }
            }
            let score: i32 = events
                .iter()
                .filter(|e| e.0 == *test)
                .map(|e| if e.1 == "definite" { 3 } else { 1 })
                .sum();
            let (length, since) = if mains.is_empty() {
                (0, None)
            } else {
                streak_at(mains.len() - 1)
            };
            let broken = length >= BROKEN_RUNS;
            let state = if broken {
                "broken"
            } else if score > 0 {
                "flaky"
            } else {
                "healthy"
            };
            states.insert(
                test.to_string(),
                (
                    state.to_string(),
                    score,
                    broken.then(|| since.unwrap().to_string()),
                    broken.then(|| mains.last().unwrap().failure_hash.clone().unwrap()),
                ),
            );
        }
        Self {
            events,
            states,
            incidents,
        }
    }
}

async fn seed_run(server: &Server, run: &GenRun) {
    let mut seeded = Run::new(&run.run_id, run.branch, run.at).hash(run.input_hash);
    seeded.cached = run.cached;
    seeded.confidence = Some(if run.partial { "partial" } else { "complete" });
    for (test, outcome, attempt, failure) in &run.attempts {
        seeded = seeded.attempt(test, outcome, *attempt, *failure);
    }
    seed(server, &seeded).await;
}

async fn observed(server: &Server) -> Model {
    let client = server.client().await;
    let events = client
        .query(
            "SELECT test_id, kind, reason, run_id FROM test_results.test_flake_events",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    let states = client
        .query(
            "SELECT test_id, state, flake_score, broken_since_sha, broken_failure_hash \
             FROM test_results.tests",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2), r.get(3), r.get(4))))
        .collect();
    let incidents = client
        .query("SELECT run_id FROM test_results.target_incidents", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    Model {
        events,
        states,
        incidents,
    }
}

/// Seed `runs` in order, classifying in passes at random times between
/// them, then once at `end`.
async fn classify_in_passes(server: &Server, runs: &[GenRun], seed: u64, end: DateTime<Utc>) {
    let mut rng = Rng::new(seed ^ 0xC1A55);
    for (i, run) in runs.iter().enumerate() {
        seed_run(server, run).await;
        let next = runs.get(i + 1).map_or(end, |r| r.at);
        if rng.chance(30) && next > run.at {
            let gap = (next - run.at).num_seconds().max(1) as u64;
            let now = run.at + Duration::seconds(rng.below(gap) as i64);
            classifier::run_once(&server.app, now)
                .await
                .unwrap()
                .unwrap();
        }
    }
    classifier::run_once(&server.app, end)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn incremental_passes_agree_with_the_rules_on_random_histories() {
    let end = Utc::now();
    for case in 0..12u64 {
        let Some(server) = start(&[]).await else {
            return;
        };
        let runs = generate(case, 70, end);
        classify_in_passes(&server, &runs, case, end).await;
        let got = observed(&server).await;
        let want = Model::new(&runs);
        assert_eq!(
            got.events,
            want.events,
            "case {case}: events differ (missing: {:?}, extra: {:?})",
            want.events.difference(&got.events).collect::<Vec<_>>(),
            got.events.difference(&want.events).collect::<Vec<_>>(),
        );
        assert_eq!(got.states, want.states, "case {case}: states differ");
        assert_eq!(
            got.incidents, want.incidents,
            "case {case}: incidents differ"
        );
        assert!(
            !want.incidents.is_empty(),
            "case {case}: no incident to check"
        );
        server.stop().await;
    }
}

#[tokio::test]
async fn one_pass_over_everything_agrees_with_the_rules() {
    // The first pass after a deploy, or after a long outage, works through
    // the whole history in chunks.
    let end = Utc::now();
    for case in 100..104u64 {
        let Some(server) = start(&[]).await else {
            return;
        };
        let runs = generate(case, 70, end);
        for run in &runs {
            seed_run(&server, run).await;
        }
        classifier::run_once(&server.app, end)
            .await
            .unwrap()
            .unwrap();
        let got = observed(&server).await;
        let want = Model::new(&runs);
        assert_eq!(got.events, want.events, "case {case}: events differ");
        assert_eq!(got.states, want.states, "case {case}: states differ");
        assert_eq!(
            got.incidents, want.incidents,
            "case {case}: incidents differ"
        );
        assert!(
            !want.incidents.is_empty(),
            "case {case}: no incident to check"
        );
        server.stop().await;
    }
}
