//! Dealing a partitioned test suite out by recorded duration.
//!
//! Each partition of a suite runs on its own runner and works out its own
//! share, so every partition must reach the same split from the same
//! inputs: the file list and the durations in [`FILE`], both from the
//! checkout. Nothing is fetched while the suite runs, so no partition can
//! see different durations from another. `aster results durations` writes
//! the file from aster-srv's medians, to be committed like any change. The
//! split is greedy longest-processing-time: the longest file first, each to
//! the partition with the least time so far.
//!
//! Without the file, or without the suite in it, the command runs as
//! written and Mix deals the files out itself, on every partition alike.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::client::{percent_encode, Server};
use super::upload::{proxy_token_from_env, Credentials};

/// The most partition targets one request names (the server's limit).
pub const MAX_TARGETS: usize = 32;
/// Requests made before giving up on the server.
const ATTEMPTS: u32 = 3;
/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Each file's median duration in milliseconds.
pub type Durations = HashMap<String, u64>;

/// Split `files` into `partitions` sets. A file without a duration counts
/// as the median of the known ones. Ties go to the earlier path and the
/// lower partition, so the split depends only on its inputs. Every
/// partition gets a file when there are at least as many files.
pub fn split(files: &[String], durations: &Durations, partitions: usize) -> Vec<Vec<String>> {
    let mut known: Vec<u64> = files
        .iter()
        .filter_map(|file| durations.get(file).copied())
        .collect();
    known.sort_unstable();
    let fallback = known.get(known.len() / 2).copied().unwrap_or(1);
    let mut weighted: Vec<(u64, &String)> = files
        .iter()
        .map(|file| (durations.get(file).copied().unwrap_or(fallback), file))
        .collect();
    weighted.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let mut bins: Vec<(u64, Vec<String>)> = vec![(0, Vec::new()); partitions];
    for (weight, file) in weighted {
        let (_, bin) = bins
            .iter_mut()
            .enumerate()
            .min_by_key(|(index, (load, files))| (*load, files.len(), *index))
            .expect("at least one partition");
        bin.0 += weight;
        bin.1.push(file.clone());
    }
    bins.into_iter()
        .map(|(_, mut files)| {
            files.sort();
            files
        })
        .collect()
}

/// The expected time of `files`, by the same weights as [`split`].
pub fn expected_ms(files: &[String], all: &[String], durations: &Durations) -> u64 {
    let mut known: Vec<u64> = all
        .iter()
        .filter_map(|f| durations.get(f).copied())
        .collect();
    known.sort_unstable();
    let fallback = known.get(known.len() / 2).copied().unwrap_or(1);
    files
        .iter()
        .map(|f| durations.get(f).copied().unwrap_or(fallback))
        .sum()
}

/// The durations file, at the workspace root.
pub const FILE: &str = "aster-durations.json";
/// The [`DurationsFile::version`] this Aster writes and reads.
const VERSION: u32 = 1;

/// [`FILE`]: each partitioned suite's file durations.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DurationsFile {
    pub version: u32,
    /// The UTC day the medians were computed as of, `YYYY-MM-DD`.
    pub day: String,
    /// By the suite's directory, relative to the workspace root.
    pub suites: BTreeMap<String, Suite>,
}

/// One partitioned suite in [`FILE`].
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Suite {
    /// The partition targets whose runs the medians come from.
    pub targets: Vec<String>,
    /// Each test file's median duration in milliseconds.
    pub files: BTreeMap<String, u64>,
}

/// The durations [`FILE`] records for the suite in `suite` (relative to
/// the workspace root). `Ok(None)`: no file, or the suite is not in it.
pub fn read(workspace_root: &Path, suite: &str) -> Result<Option<Durations>> {
    let path = workspace_root.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {FILE}")),
    };
    let file: DurationsFile =
        serde_json::from_slice(&bytes).with_context(|| format!("cannot parse {FILE}"))?;
    if file.version != VERSION {
        bail!(
            "{FILE} is version {}, this Aster reads {VERSION}",
            file.version
        );
    }
    Ok(file
        .suites
        .get(suite)
        .map(|suite| suite.files.iter().map(|(f, ms)| (f.clone(), *ms)).collect()))
}

/// Write `file` as [`FILE`], sorted and pretty, so a refresh diffs cleanly.
pub fn write(workspace_root: &Path, file: &DurationsFile) -> Result<()> {
    let mut text = serde_json::to_string_pretty(file)?;
    text.push('\n');
    std::fs::write(workspace_root.join(FILE), text).with_context(|| format!("cannot write {FILE}"))
}

/// A [`DurationsFile`] for `suites` (each a directory and its partition
/// targets), with aster-srv's medians as of `day`.
pub fn fetch_all(
    url: &str,
    suites: &[(String, Vec<String>)],
    day: NaiveDate,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<DurationsFile> {
    let Some(credentials) = Credentials::from_env(env) else {
        bail!("no credentials for {url}: in GitHub Actions grant `id-token: write`; elsewhere set ASTER_SRV_TOKEN");
    };
    let mut file = DurationsFile {
        version: VERSION,
        day: day.to_string(),
        suites: BTreeMap::new(),
    };
    for (dir, targets) in suites {
        let mut targets = targets.clone();
        targets.sort();
        targets.dedup();
        if targets.len() > MAX_TARGETS {
            bail!(
                "{dir} has {} partition targets; aster-srv takes at most {MAX_TARGETS}",
                targets.len()
            );
        }
        let files = fetch(url, &credentials, env, &targets, day)
            .with_context(|| format!("durations of {dir}"))?;
        file.suites.insert(dir.clone(), Suite { targets, files });
    }
    Ok(file)
}

/// The medians aster-srv reports for the files `targets` ran, as of `day`.
fn fetch(
    url: &str,
    credentials: &Credentials,
    env: &dyn Fn(&str) -> Option<String>,
    targets: &[String],
    day: NaiveDate,
) -> Result<BTreeMap<String, u64>> {
    #[derive(Deserialize)]
    struct File {
        file: String,
        p50_ms: u64,
    }
    let mut path = format!("/v1/test-results/durations?day={day}");
    for target in targets {
        path.push_str(&format!("&target={}", percent_encode(target)));
    }
    let mut attempt = 1;
    let body = loop {
        let result =
            Server::connect_with_timeout(url, credentials, proxy_token_from_env(env), TIMEOUT)
                .and_then(|server| server.get_json(&path));
        match result {
            Ok(Some(body)) => break body,
            Ok(None) => bail!("{url} does not serve durations"),
            Err(error) if attempt >= ATTEMPTS => return Err(error),
            Err(_) => {
                std::thread::sleep(Duration::from_secs(u64::from(attempt)));
                attempt += 1;
            }
        }
    };
    let files: Vec<File> = serde_json::from_value(body["files"].clone())
        .context("cannot read the server's durations")?;
    Ok(files.into_iter().map(|f| (f.file, f.p50_ms)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn longest_files_first_to_the_least_loaded_partition() {
        let files = names(&["a", "b", "c", "d", "e", "f"]);
        let durations: Durations = [
            ("a", 70),
            ("b", 50),
            ("c", 40),
            ("d", 30),
            ("e", 20),
            ("f", 10),
        ]
        .into_iter()
        .map(|(f, ms)| (f.to_string(), ms))
        .collect();
        let bins = split(&files, &durations, 3);
        // a(70) | b(50)+e(20) | c(40)+d(30), then f(10) to the first at 70.
        assert_eq!(
            bins,
            [names(&["a", "f"]), names(&["b", "e"]), names(&["c", "d"])]
        );
        let all: Vec<String> = bins.concat();
        assert_eq!(all.len(), files.len());
        assert_eq!(expected_ms(&bins[1], &files, &durations), 70);
    }

    #[test]
    fn unknown_files_count_as_the_median_and_ties_are_stable() {
        let files = names(&["a", "b", "c", "new1", "new2"]);
        let durations: Durations = [("a", 10), ("b", 20), ("c", 30)]
            .into_iter()
            .map(|(f, ms)| (f.to_string(), ms))
            .collect();
        assert_eq!(expected_ms(&names(&["new1"]), &files, &durations), 20);
        let first = split(&files, &durations, 2);
        let mut shuffled = files.clone();
        shuffled.reverse();
        assert_eq!(split(&shuffled, &durations, 2), first);
        // With no durations at all every file weighs the same.
        let even = split(&files, &Durations::new(), 2);
        assert_eq!(even, [names(&["a", "c", "new2"]), names(&["b", "new1"])]);
    }

    #[test]
    fn every_partition_gets_a_file_even_at_zero_cost() {
        let files = names(&["a", "b", "c"]);
        let zero: Durations = files.iter().map(|f| (f.clone(), 0)).collect();
        let bins = split(&files, &zero, 3);
        assert!(bins.iter().all(|bin| bin.len() == 1), "{bins:?}");
    }

    #[test]
    fn the_file_round_trips_and_names_suites_by_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path(), "services/platform").unwrap(), None);
        let file = DurationsFile {
            version: VERSION,
            day: "2026-10-15".into(),
            suites: [(
                "services/platform".to_string(),
                Suite {
                    targets: names(&["//services/platform-test-shard-1:test-ci"]),
                    files: [("test/a_test.exs".to_string(), 42)].into_iter().collect(),
                },
            )]
            .into_iter()
            .collect(),
        };
        write(dir.path(), &file).unwrap();
        let found = read(dir.path(), "services/platform").unwrap().unwrap();
        assert_eq!(found["test/a_test.exs"], 42);
        assert_eq!(read(dir.path(), "services/other").unwrap(), None);
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
    }

    #[test]
    fn a_broken_or_newer_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{").unwrap();
        assert!(read(dir.path(), "a").is_err());
        std::fs::write(
            dir.path().join(FILE),
            r#"{"version":2,"day":"x","suites":{}}"#,
        )
        .unwrap();
        let error = read(dir.path(), "a").unwrap_err().to_string();
        assert!(error.contains("version 2"), "{error}");
    }

    #[test]
    fn fetching_needs_credentials() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let suites = vec![("app".to_string(), names(&["//a:test"]))];
        let error = fetch_all("http://127.0.0.1:9", &suites, day, &|_| None).unwrap_err();
        assert!(error.to_string().contains("no credentials"), "{error}");
    }
}
