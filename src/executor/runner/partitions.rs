//! Partitioned test suites: whose tests a shard runs, and which of them.
//!
//! A shard project (`//services/platform-test-shard-2`) runs one partition
//! of another project's suite. Its tests are recorded under the project
//! that owns them, so a test keeps its identity whichever shard runs it.
//! And when the workspace's durations file (written from aster-srv by
//! `aster results durations`) times the suite, the shard runs a share picked
//! by time instead of the one Mix deals out by count.

use std::collections::HashMap;
use std::path::Path;

use crate::discovery::DiscoveredProject;
use crate::plugins::mix_partitions;
use crate::test_results::balance;

/// `ASTER_BALANCE_PARTITIONS=off` keeps Mix's own split.
pub(super) const OFF_SWITCH: &str = "ASTER_BALANCE_PARTITIONS";

/// The address of the project whose tests `command` runs, when that is not
/// `project` (a `cd ../platform && mix test` one-liner).
pub(super) fn test_owner(
    project: &DiscoveredProject,
    target: &crate::plugins::Target,
    command: &str,
    project_map: &HashMap<String, &DiscoveredProject>,
) -> Option<String> {
    let owner = super::tests_run_elsewhere(project, target, command, project_map)??;
    project_map
        .iter()
        .find(|(_, candidate)| std::ptr::eq(**candidate, owner))
        .map(|(address, _)| address.clone())
}

/// Every partitioned Mix suite in the workspace: its directory relative to
/// `workspace_root` and the targets that run a partition of it, sorted.
pub fn partitioned_suites(
    workspace_root: &Path,
    project_map: &HashMap<String, &DiscoveredProject>,
) -> Vec<(String, Vec<String>)> {
    let mut suites: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (address, project) in project_map {
        for (name, target) in &project.targets {
            let working_dir = target.working_dir.as_deref().unwrap_or(&project.root);
            let Some(found) = mix_partitions::parse(working_dir, &target.command) else {
                continue;
            };
            if let Some(key) = suite_key(workspace_root, &found.dir) {
                suites
                    .entry(key)
                    .or_default()
                    .push(format!("{address}:{name}"));
            }
        }
    }
    suites
        .into_iter()
        .map(|(key, mut targets)| {
            targets.sort();
            (key, targets)
        })
        .collect()
}

/// A suite's key in the durations file: its directory relative to the
/// workspace root, with `/` separators.
fn suite_key(workspace_root: &Path, dir: &Path) -> Option<String> {
    let relative = canonical(dir)
        .strip_prefix(canonical(workspace_root))
        .ok()?
        .to_path_buf();
    let parts: Option<Vec<&str>> = relative
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect();
    Some(parts?.join("/"))
}

fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The command that runs `address`'s share of a partitioned suite by
/// recorded duration, or `None` to run `commands` as written. Says which
/// split it used.
pub(super) fn balance(
    address: &str,
    commands: &[String],
    working_dir: &Path,
    workspace_root: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let [command] = commands else {
        return None;
    };
    let found = mix_partitions::parse(working_dir, command)?;
    let label = format!(
        "[aster] {address}: partition {}/{}",
        found.partition, found.total
    );
    let keep = |why: &str| {
        eprintln!("{label} split by Mix ({why})");
        None
    };
    if env(OFF_SWITCH).is_some_and(|value| value.eq_ignore_ascii_case("off")) {
        return keep(&format!("{OFF_SWITCH}=off"));
    }
    let Some(suite) = suite_key(workspace_root, &found.dir) else {
        return keep("its suite is outside the workspace");
    };
    let durations = match balance::read(workspace_root, &suite) {
        Ok(Some(durations)) => durations,
        Ok(None) => return keep(&format!("no durations for {suite} in {}", balance::FILE)),
        Err(error) => return keep(&format!("{error:#}")),
    };
    let Some(files) = found.files() else {
        return keep("cannot list its files for sure");
    };
    if files.len() < found.total {
        return keep("fewer files than partitions");
    }
    let known = files.iter().filter(|f| durations.contains_key(*f)).count();
    if known == 0 {
        return keep(&format!("none of its files are timed in {}", balance::FILE));
    }
    let bins = balance::split(&files, &durations, found.total);
    let mine = &bins[found.partition - 1];
    let expected = balance::expected_ms(mine, &files, &durations);
    let slowest = bins
        .iter()
        .map(|bin| balance::expected_ms(bin, &files, &durations))
        .max()
        .unwrap_or(0);
    eprintln!(
        "{label} balanced by duration: {} of {} files, ~{}s of a slowest ~{}s \
         ({known} files timed in {})",
        mine.len(),
        files.len(),
        expected / 1000,
        slowest / 1000,
        balance::FILE,
    );
    Some(found.render(mine))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{ProjectMetadata, Target};

    fn project(root: std::path::PathBuf, targets: HashMap<String, Target>) -> DiscoveredProject {
        DiscoveredProject {
            config_path: root.join("mix.exs"),
            root,
            metadata: ProjectMetadata {
                name: "p".to_string(),
                version: None,
            },
            dependencies: vec![],
            targets,
            plugin_name: "elixir".to_string(),
            languages: vec![],
            build_system: None,
            relative_path: std::path::PathBuf::new(),
        }
    }

    fn shard(root: &Path, partition: usize) -> (String, DiscoveredProject) {
        let dir = root.join(format!("shard-{partition}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut targets = HashMap::new();
        targets.insert(
            "test-ci".to_string(),
            Target {
                command: format!(
                    "bash -c 'cd ../app && MIX_TEST_PARTITION={partition} exec mix test --partitions 2'"
                ),
                ..Default::default()
            },
        );
        (format!("//shard-{partition}"), project(dir, targets))
    }

    #[test]
    fn shards_name_their_suites_owner_and_find_each_other() {
        let ws = tempfile::tempdir().unwrap();
        let app_dir = ws.path().join("app");
        std::fs::create_dir_all(app_dir.join("test")).unwrap();
        let app = project(app_dir, HashMap::new());
        let (a1, s1) = shard(ws.path(), 1);
        let (a2, s2) = shard(ws.path(), 2);
        let map: HashMap<String, &DiscoveredProject> = [
            ("//app".to_string(), &app),
            (a1.clone(), &s1),
            (a2.clone(), &s2),
        ]
        .into_iter()
        .collect();
        let target = &s1.targets["test-ci"];
        assert_eq!(
            test_owner(&s1, target, &target.command, &map).as_deref(),
            Some("//app")
        );
        assert_eq!(test_owner(&app, target, "mix test", &map), None);

        assert_eq!(
            partitioned_suites(ws.path(), &map),
            [(
                "app".to_string(),
                vec![
                    "//shard-1:test-ci".to_string(),
                    "//shard-2:test-ci".to_string()
                ]
            )]
        );
    }

    #[test]
    fn without_durations_or_when_switched_off_mix_splits() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("app/test")).unwrap();
        for file in ["a", "b"] {
            std::fs::write(ws.path().join(format!("app/test/{file}_test.exs")), "").unwrap();
        }
        std::fs::write(ws.path().join("app/mix.exs"), "[app: :app]").unwrap();
        let (address, s1) = shard(ws.path(), 1);
        let command = vec![s1.targets["test-ci"].command.clone()];
        let address = format!("{address}:test-ci");
        let none = |_: &str| None;
        // No durations file.
        assert_eq!(
            balance(&address, &command, &s1.root, ws.path(), &none),
            None
        );
        // A file without this suite.
        write_durations(ws.path(), "other", &[("test/a_test.exs", 5)]);
        assert_eq!(
            balance(&address, &command, &s1.root, ws.path(), &none),
            None
        );
        // A broken file.
        std::fs::write(ws.path().join(balance::FILE), "{").unwrap();
        assert_eq!(
            balance(&address, &command, &s1.root, ws.path(), &none),
            None
        );
        // Timed, but switched off.
        write_durations(ws.path(), "app", &[("test/a_test.exs", 5)]);
        let off = |name: &str| (name == OFF_SWITCH).then(|| "off".to_string());
        assert_eq!(balance(&address, &command, &s1.root, ws.path(), &off), None);
        assert!(balance(&address, &command, &s1.root, ws.path(), &none).is_some());
        // Not a partitioned command at all.
        let plain = vec!["mix test".to_string()];
        assert_eq!(balance(&address, &plain, &s1.root, ws.path(), &none), None);
    }

    fn write_durations(root: &Path, suite: &str, files: &[(&str, u64)]) {
        let file = balance::DurationsFile {
            version: 1,
            day: "2026-10-15".into(),
            suites: [(
                suite.to_string(),
                balance::Suite {
                    targets: vec![],
                    files: files.iter().map(|(f, ms)| (f.to_string(), *ms)).collect(),
                },
            )]
            .into_iter()
            .collect(),
        };
        balance::write(root, &file).unwrap();
    }

    #[test]
    fn every_shard_takes_a_disjoint_share_of_the_suite_by_duration() {
        let ws = tempfile::tempdir().unwrap();
        let app_dir = ws.path().join("app");
        std::fs::create_dir_all(app_dir.join("test")).unwrap();
        for file in ["a", "b", "c", "d"] {
            std::fs::write(app_dir.join(format!("test/{file}_test.exs")), "").unwrap();
        }
        std::fs::write(app_dir.join("mix.exs"), "[app: :app]").unwrap();
        let (_, s1) = shard(ws.path(), 1);
        let (_, s2) = shard(ws.path(), 2);
        write_durations(
            ws.path(),
            "app",
            &[
                ("test/a_test.exs", 100),
                ("test/b_test.exs", 10),
                ("test/c_test.exs", 10),
                ("test/d_test.exs", 80),
                ("test/gone_test.exs", 999),
            ],
        );
        // No server and no network: the checkout alone decides.
        let none = |_: &str| None;
        let run = |shard: &DiscoveredProject, address: &str| {
            let command = vec![shard.targets["test-ci"].command.clone()];
            balance(address, &command, &shard.root, ws.path(), &none).unwrap()
        };
        assert_eq!(
            run(&s1, "//shard-1:test-ci"),
            "bash -c 'cd ../app && MIX_TEST_PARTITION=1 exec mix test test/a_test.exs'"
        );
        assert_eq!(
            run(&s2, "//shard-2:test-ci"),
            "bash -c 'cd ../app && MIX_TEST_PARTITION=2 exec mix test test/b_test.exs test/c_test.exs test/d_test.exs'"
        );
    }
}
