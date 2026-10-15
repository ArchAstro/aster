//! Partitioned test suites: whose tests a shard runs, and which of them.
//!
//! A shard project (`//services/platform-test-shard-2`) runs one partition
//! of another project's suite. Its tests are recorded under the project
//! that owns them, so a test keeps its identity whichever shard runs it.
//! And when aster-srv knows how long each file takes, the shard runs a
//! share picked by time instead of the one Mix deals out by count.

use std::collections::HashMap;
use std::path::Path;

use crate::discovery::DiscoveredProject;
use crate::plugins::mix_partitions::{self, PartitionedMix};
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

/// The targets that run a partition of the same suite as `found`, `found`'s
/// own included, sorted.
fn peers(found: &PartitionedMix, project_map: &HashMap<String, &DiscoveredProject>) -> Vec<String> {
    let dir = canonical(&found.dir);
    let dir = &dir;
    let mut peers: Vec<String> = project_map
        .iter()
        .flat_map(|(address, project)| {
            project.targets.iter().filter_map(move |(name, target)| {
                let working_dir = target.working_dir.as_deref().unwrap_or(&project.root);
                let other = mix_partitions::parse(working_dir, &target.command)?;
                (other.total == found.total && canonical(&other.dir) == *dir)
                    .then(|| format!("{address}:{name}"))
            })
        })
        .collect();
    peers.sort();
    peers
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
    project_map: &HashMap<String, &DiscoveredProject>,
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
    if env("ASTER_SRV_URL").is_none_or(|url| url.is_empty()) {
        return keep("no ASTER_SRV_URL for durations");
    }
    let Some(files) = found.files() else {
        return keep("cannot list its files for sure");
    };
    if files.len() < found.total {
        return keep("fewer files than partitions");
    }
    let peers = peers(&found, project_map);
    if !peers.iter().any(|peer| peer == address) {
        return keep("this target is not one of the suite's partitions");
    }
    if peers.len() > crate::test_results::balance::MAX_TARGETS {
        return keep("too many partition targets");
    }
    let Some(day) = balance::commit_day(workspace_root) else {
        return keep("no commit date");
    };
    let durations = match balance::fetch(workspace_root, &peers, day, env) {
        Ok(Some(durations)) => durations,
        Ok(None) => return keep("no ASTER_SRV_URL for durations"),
        Err(error) => return keep(&format!("durations unavailable: {error:#}")),
    };
    let known = files.iter().filter(|f| durations.contains_key(*f)).count();
    if known == 0 {
        return keep("no recorded durations");
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
         ({known} files timed, medians before {day})",
        mine.len(),
        files.len(),
        expected / 1000,
        slowest / 1000,
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

        let found = mix_partitions::parse(&s1.root, &target.command).unwrap();
        assert_eq!(
            peers(&found, &map),
            ["//shard-1:test-ci", "//shard-2:test-ci"]
        );
    }

    #[test]
    fn without_a_server_or_when_switched_off_mix_splits() {
        let ws = tempfile::tempdir().unwrap();
        let (address, s1) = shard(ws.path(), 1);
        let map: HashMap<String, &DiscoveredProject> =
            [(address.clone(), &s1)].into_iter().collect();
        let command = vec![s1.targets["test-ci"].command.clone()];
        let address = format!("{address}:test-ci");
        let none = |_: &str| None;
        assert_eq!(
            balance(&address, &command, &s1.root, ws.path(), &map, &none),
            None
        );
        let off = |name: &str| match name {
            OFF_SWITCH => Some("off".to_string()),
            "ASTER_SRV_URL" => Some("http://127.0.0.1:9".to_string()),
            _ => None,
        };
        assert_eq!(
            balance(&address, &command, &s1.root, ws.path(), &map, &off),
            None
        );
        // Not a partitioned command at all.
        let plain = vec!["mix test".to_string()];
        assert_eq!(
            balance(&address, &plain, &s1.root, ws.path(), &map, &none),
            None
        );
    }

    /// Answers one request with `body`; returns the request line.
    fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            request_line
        });
        (url, handle)
    }

    #[test]
    fn every_shard_takes_a_disjoint_share_of_the_suite_by_duration() {
        let ws = tempfile::tempdir().unwrap();
        let vcs = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(ws.path())
                .env("GIT_COMMITTER_DATE", "2026-10-14T23:30:00-02:00")
                .env("GIT_AUTHOR_DATE", "2026-10-14T23:30:00-02:00")
                .status()
                .unwrap();
            assert!(status.success());
        };
        vcs(&["init", "-q"]);
        vcs(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let app_dir = ws.path().join("app");
        std::fs::create_dir_all(app_dir.join("test")).unwrap();
        for file in ["a", "b", "c", "d"] {
            std::fs::write(app_dir.join(format!("test/{file}_test.exs")), "").unwrap();
        }
        std::fs::write(app_dir.join("mix.exs"), "[app: :app]").unwrap();
        let app = project(app_dir, HashMap::new());
        let (a1, s1) = shard(ws.path(), 1);
        let (a2, s2) = shard(ws.path(), 2);
        let map: HashMap<String, &DiscoveredProject> =
            [("//app".to_string(), &app), (a1, &s1), (a2, &s2)]
                .into_iter()
                .collect();
        let (url, server) = serve_once(
            r#"{"files":[{"file":"test/a_test.exs","p50_ms":100,"runs":3},{"file":"test/b_test.exs","p50_ms":10,"runs":3},{"file":"test/c_test.exs","p50_ms":10,"runs":3},{"file":"test/d_test.exs","p50_ms":80,"runs":3},{"file":"test/gone_test.exs","p50_ms":999,"runs":3}]}"#,
        );
        let env = move |name: &str| match name {
            "ASTER_SRV_URL" => Some(url.clone()),
            "ASTER_SRV_TOKEN" => Some("t".to_string()),
            _ => None,
        };
        let run = |shard: &DiscoveredProject, address: &str| {
            let command = vec![shard.targets["test-ci"].command.clone()];
            balance(address, &command, &shard.root, ws.path(), &map, &env).unwrap()
        };
        let first = run(&s1, "//shard-1:test-ci");
        // The second shard reads the first's cached answer: one request.
        let second = run(&s2, "//shard-2:test-ci");
        let request = server.join().unwrap();
        assert!(request.contains("day=2026-10-15"), "{request}");
        assert!(
            request.contains("target=%2F%2Fshard-1%3Atest-ci&target=%2F%2Fshard-2%3Atest-ci"),
            "{request}"
        );
        assert_eq!(
            first,
            "bash -c 'cd ../app && MIX_TEST_PARTITION=1 exec mix test test/a_test.exs'"
        );
        assert_eq!(
            second,
            "bash -c 'cd ../app && MIX_TEST_PARTITION=2 exec mix test test/b_test.exs test/c_test.exs test/d_test.exs'"
        );
    }
}
