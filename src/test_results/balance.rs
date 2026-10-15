//! Dealing a partitioned test suite out by recorded duration.
//!
//! Each partition of a suite runs on its own runner and works out its own
//! share, so every partition must reach the same split from the same
//! inputs: the file list (from the checkout), and each file's median
//! duration as aster-srv reported it for the UTC day of the commit (which
//! the server answers the same way however often it is asked). The split
//! is greedy longest-processing-time: the longest file first, each to the
//! partition with the least time so far.
//!
//! Durations are a hint. Without them (no server, no history, an error)
//! the command runs as written, and Mix deals the files out itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// The UTC day of `HEAD`'s commit, the same on every runner of that commit.
pub fn commit_day(workspace_root: &Path) -> Option<NaiveDate> {
    let output = std::process::Command::new("git")
        .args(["log", "-1", "--format=%cI", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let at = chrono::DateTime::parse_from_rfc3339(text.trim()).ok()?;
    Some(at.with_timezone(&chrono::Utc).date_naive())
}

#[derive(Serialize, Deserialize)]
struct Cached {
    targets: Vec<String>,
    /// `YYYY-MM-DD`.
    day: String,
    files: Vec<CachedFile>,
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    file: String,
    p50_ms: u64,
}

fn cache_path(workspace_root: &Path, targets: &[String], day: NaiveDate) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for target in targets {
        hasher.update(target.as_bytes());
        hasher.update([0]);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    workspace_root
        .join(".aster")
        .join("durations")
        .join(format!("{day}-{}.json", &digest[..16]))
}

/// The durations of the files `targets` ran, as of `day`. `Ok(None)`: no
/// server is configured. Answers are kept under `.aster/durations/`, since
/// the server's answer for a day does not change.
pub fn fetch(
    workspace_root: &Path,
    targets: &[String],
    day: NaiveDate,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Durations>> {
    let Some(url) = env("ASTER_SRV_URL").filter(|url| !url.is_empty()) else {
        return Ok(None);
    };
    let mut targets = targets.to_vec();
    targets.sort();
    targets.dedup();
    let cache = cache_path(workspace_root, &targets, day);
    if let Some(found) = std::fs::read(&cache)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cached>(&bytes).ok())
        .filter(|cached| cached.targets == targets && cached.day == day.to_string())
    {
        return Ok(Some(
            found
                .files
                .into_iter()
                .map(|f| (f.file, f.p50_ms))
                .collect(),
        ));
    }
    let Some(credentials) = Credentials::from_env(env) else {
        bail!("no credentials for {url}");
    };
    let mut path = format!("/v1/test-results/durations?day={day}");
    for target in &targets {
        path.push_str(&format!("&target={}", percent_encode(target)));
    }
    let mut attempt = 1;
    let body = loop {
        let result =
            Server::connect_with_timeout(&url, &credentials, proxy_token_from_env(env), TIMEOUT)
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
    let files: Vec<CachedFile> = serde_json::from_value(body["files"].clone())
        .context("cannot read the server's durations")?;
    let cached = Cached {
        targets,
        day: day.to_string(),
        files,
    };
    if let Some(parent) = cache.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            if let Ok(bytes) = serde_json::to_vec(&cached) {
                let _ = std::fs::write(&cache, bytes);
            }
        }
    }
    Ok(Some(
        cached
            .files
            .into_iter()
            .map(|f| (f.file, f.p50_ms))
            .collect(),
    ))
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
    fn no_server_means_no_durations() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let found = fetch(dir.path(), &names(&["//a:test"]), day, &|_| None).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn a_cached_answer_is_used_without_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let targets = names(&["//b:test", "//a:test"]);
        let mut sorted = targets.clone();
        sorted.sort();
        let path = cache_path(dir.path(), &sorted, day);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&Cached {
                targets: sorted,
                day: day.to_string(),
                files: vec![CachedFile {
                    file: "test/a_test.exs".into(),
                    p50_ms: 42,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        // The URL is unreachable and there are no credentials: only the
        // cache can answer.
        let env = |name: &str| (name == "ASTER_SRV_URL").then(|| "http://127.0.0.1:9".to_string());
        let found = fetch(dir.path(), &targets, day, &env).unwrap().unwrap();
        assert_eq!(found["test/a_test.exs"], 42);
    }
}
