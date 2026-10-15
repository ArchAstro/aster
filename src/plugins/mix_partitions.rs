//! `MIX_TEST_PARTITION=K mix test --partitions N`, run as an explicit list
//! of test files instead.
//!
//! Mix deals the sorted test files out in turn, so a partition's share of
//! the suite's time is luck. Aster can instead pick partition K's files
//! itself (see [`crate::test_results::balance`]) and run them by name. This
//! module recognises such a command, lists the files Mix would deal out,
//! and writes the command that runs a chosen set of them.
//!
//! Every partition must list exactly the files Mix would, or a file runs on
//! no partition. So anything that changes Mix's own selection (custom
//! `test_paths` or `test_pattern`, umbrella apps, line filters, `--failed`,
//! `--stale`, a symlink under a test directory) makes [`parse`] or
//! [`PartitionedMix::files`] say `None`, and the command runs unchanged.
//!
//! `--partitions` is dropped but `MIX_TEST_PARTITION` is kept: projects
//! name per-partition databases after it, and without `--partitions` Mix
//! ignores it.

use std::path::{Path, PathBuf};

use super::related_tests::{Argv, Delegated};

const MIX_VALUE_FLAGS: &[&str] = &[
    "--only",
    "--exclude",
    "--include",
    "--seed",
    "--max-failures",
    "--partitions",
    "--timeout",
    "--formatter",
    "--slowest",
    "--max-cases",
    "--cover-filter",
    "--exit-status",
];

/// `mix.exs` settings that change which files `mix test` runs.
const SELECTION_SETTINGS: &[&str] = &[
    "test_paths",
    "test_pattern",
    "test_load_filters",
    "test_ignore_filters",
    "apps_path",
];

/// A partitioned `mix test`, possibly delegated (`bash -c 'cd ../core &&
/// MIX_TEST_PARTITION=1 exec mix test --partitions 4'`).
pub(crate) struct PartitionedMix {
    delegated: Option<Delegated>,
    argv: Argv,
    test: usize,
    /// `N` of `--partitions N`.
    pub(crate) total: usize,
    /// `K` of `MIX_TEST_PARTITION=K`, from 1.
    pub(crate) partition: usize,
    /// Where `mix test` runs.
    pub(crate) dir: PathBuf,
}

/// The partitioned `mix test` that `command` runs in `working_dir`.
pub(crate) fn parse(working_dir: &Path, command: &str) -> Option<PartitionedMix> {
    let (delegated, inner) = match Argv::parse(command) {
        Some(argv) => (None, argv),
        None => {
            let delegated = Delegated::parse(command)?;
            let inner = Argv::parse(&delegated.command)?;
            (Some(delegated), inner)
        }
    };
    let argv = inner;
    if argv.parts[argv.program] != "mix" || argv.parts.iter().any(|p| p.ends_with(',')) {
        return None;
    }
    let test = argv.program
        + 1
        + argv.parts[argv.program + 1..]
            .iter()
            .position(|p| p == "test")?;
    // `mix test` must be the task, not an argument of another one.
    if test != argv.program + 1 {
        return None;
    }
    let arguments = &argv.parts[test + 1..];
    if arguments
        .iter()
        .any(|p| p == "--failed" || p == "--stale" || p == "--")
    {
        return None;
    }
    let total: usize = arguments.iter().enumerate().find_map(|(i, part)| {
        match part.strip_prefix("--partitions") {
            Some("") => arguments.get(i + 1)?.parse().ok(),
            Some(rest) => rest.strip_prefix('=')?.parse().ok(),
            None => None,
        }
    })?;
    let mut assignments: Vec<String> = argv.parts[..argv.program].to_vec();
    if let Some(delegated) = &delegated {
        assignments.extend(
            delegated
                .environment()
                .split_whitespace()
                .map(str::to_string),
        );
    }
    let partition: usize = assignments
        .iter()
        .rev()
        .find_map(|part| part.strip_prefix("MIX_TEST_PARTITION="))?
        .parse()
        .ok()?;
    if total < 2 || !(1..=total).contains(&partition) {
        return None;
    }
    let dir = match delegated.as_ref().and_then(|d| d.dir.as_deref()) {
        Some(dir) => working_dir.join(dir),
        None => working_dir.to_path_buf(),
    };
    Some(PartitionedMix {
        delegated,
        argv,
        test,
        total,
        partition,
        dir,
    })
}

impl PartitionedMix {
    fn listed(&self) -> Vec<usize> {
        self.argv.positionals(self.test + 1, MIX_VALUE_FLAGS)
    }

    /// Every file the partitions deal out, sorted, relative to [`Self::dir`]
    /// as ExUnit names them. `None` when that cannot be known for sure.
    pub(crate) fn files(&self) -> Option<Vec<String>> {
        let mix_exs = std::fs::read_to_string(self.dir.join("mix.exs")).ok()?;
        if SELECTION_SETTINGS.iter().any(|s| mix_exs.contains(s)) {
            return None;
        }
        let listed: Vec<String> = self
            .listed()
            .into_iter()
            .map(|i| self.argv.parts[i].clone())
            .collect();
        let roots = if listed.is_empty() {
            vec!["test".to_string()]
        } else {
            listed
        };
        let mut files = Vec::new();
        for root in roots {
            let root = root.trim_start_matches("./").trim_end_matches('/');
            if root.is_empty()
                || root == "."
                || root.contains(':')
                || root.starts_with('/')
                || root.split('/').any(|part| part == "..")
            {
                return None;
            }
            let path = self.dir.join(root);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if meta.is_file() {
                if !root.ends_with(".exs") {
                    return None;
                }
                files.push(root.to_string());
            } else if meta.is_dir() {
                // Path.wildcard skips hidden entries.
                let entries = walkdir::WalkDir::new(&path)
                    .sort_by_file_name()
                    .into_iter()
                    .filter_entry(|e| {
                        e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.')
                    });
                for entry in entries {
                    let entry = entry.ok()?;
                    if entry.path_is_symlink() {
                        return None;
                    }
                    let name = entry.file_name().to_string_lossy();
                    let relative = entry.path().strip_prefix(&self.dir).ok()?;
                    let relative = relative.to_str()?.replace('\\', "/");
                    if entry.file_type().is_file() && name.ends_with("_test.exs") {
                        files.push(relative);
                    }
                }
            } else {
                return None;
            }
        }
        files.sort();
        files.dedup();
        Some(files)
    }

    /// The command that runs `files` (relative to [`Self::dir`]) without
    /// `--partitions`, otherwise as written.
    pub(crate) fn render(&self, files: &[String]) -> String {
        let mut drop = self.listed();
        let parts = &self.argv.parts;
        let mut i = self.test + 1;
        while i < parts.len() {
            if parts[i] == "--partitions" {
                drop.extend([i, i + 1]);
                i += 1;
            } else if parts[i].starts_with("--partitions=") {
                drop.push(i);
            }
            i += 1;
        }
        let inner = self.argv.render(&drop, files);
        match &self.delegated {
            Some(delegated) => delegated.render(&inner),
            None => inner,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[&str], mix_exs: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mix.exs"), mix_exs).unwrap();
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        dir
    }

    const MIX_EXS: &str = "defmodule P.MixProject do\n  def project, do: [app: :p]\nend\n";

    #[test]
    fn reads_the_partition_from_a_delegated_one_liner() {
        let ws = tempfile::tempdir().unwrap();
        let shard = ws.path().join("shard");
        let found = parse(
            &shard,
            "bash -c 'cd ../platform && MIX_TEST_PARTITION=3 exec mix test --warnings-as-errors --partitions 4'",
        )
        .unwrap();
        assert_eq!((found.partition, found.total), (3, 4));
        assert_eq!(found.dir, shard.join("../platform"));
        assert_eq!(
            found.render(&["test/a_test.exs".into(), "test/b c_test.exs".into()]),
            "bash -c 'cd ../platform && MIX_TEST_PARTITION=3 exec mix test --warnings-as-errors test/a_test.exs '\\''test/b c_test.exs'\\'''"
        );

        let plain = parse(
            ws.path(),
            "MIX_TEST_PARTITION=1 mix test --partitions=2 --seed 0",
        )
        .unwrap();
        assert_eq!((plain.partition, plain.total), (1, 2));
        assert_eq!(
            plain.render(&["test/a_test.exs".into()]),
            "MIX_TEST_PARTITION=1 mix test --seed 0 test/a_test.exs"
        );
    }

    #[test]
    fn leaves_other_commands_alone() {
        let dir = Path::new("/ws");
        for command in [
            "mix test",
            "mix test --partitions 4",
            "MIX_TEST_PARTITION=1 mix test",
            "MIX_TEST_PARTITION=5 mix test --partitions 4",
            "MIX_TEST_PARTITION=1 mix test --partitions 1",
            "MIX_TEST_PARTITION=1 mix test --partitions 2 --failed",
            "MIX_TEST_PARTITION=1 mix test --partitions 2 --stale",
            "MIX_TEST_PARTITION=1 mix do compile, test --partitions 2",
            "MIX_TEST_PARTITION=1 mix coveralls test --partitions 2",
            "MIX_TEST_PARTITION=1 go test --partitions 2",
        ] {
            assert!(parse(dir, command).is_none(), "{command}");
        }
    }

    #[test]
    fn lists_the_files_mix_would_deal_out() {
        let dir = project(
            &[
                "test/b_test.exs",
                "test/a/z_test.exs",
                "test/support/helper.exs",
                "test/test_helper.exs",
                "test/.hidden/x_test.exs",
                "test/.y_test.exs",
                "other/q_test.exs",
            ],
            MIX_EXS,
        );
        let all = parse(dir.path(), "MIX_TEST_PARTITION=1 mix test --partitions 2").unwrap();
        assert_eq!(
            all.files().unwrap(),
            ["test/a/z_test.exs", "test/b_test.exs"]
        );
        let listed = parse(
            dir.path(),
            "MIX_TEST_PARTITION=1 mix test --partitions 2 --only slow ./test/a/ other/q_test.exs",
        )
        .unwrap();
        assert_eq!(
            listed.files().unwrap(),
            ["other/q_test.exs", "test/a/z_test.exs"]
        );
        assert_eq!(
            listed.render(&["test/a/z_test.exs".into()]),
            "MIX_TEST_PARTITION=1 mix test --only slow test/a/z_test.exs"
        );
    }

    #[test]
    fn unknowable_selections_are_none() {
        let custom = project(&["test/a_test.exs"], "[test_paths: [\"t\"]]");
        let found = parse(
            custom.path(),
            "MIX_TEST_PARTITION=1 mix test --partitions 2",
        )
        .unwrap();
        assert!(found.files().is_none());

        let dir = project(&["test/a_test.exs"], MIX_EXS);
        for listed in [
            "test/a_test.exs:3",
            "../x",
            "/abs",
            "missing",
            "test/a_test.ex",
        ] {
            let command = format!("MIX_TEST_PARTITION=1 mix test --partitions 2 {listed}");
            let found = parse(dir.path(), &command).unwrap();
            assert!(found.files().is_none(), "{listed}");
        }

        #[cfg(unix)]
        {
            let linked = project(&["elsewhere/x_test.exs"], MIX_EXS);
            std::fs::create_dir_all(linked.path().join("test")).unwrap();
            std::os::unix::fs::symlink(
                linked.path().join("elsewhere"),
                linked.path().join("test/link"),
            )
            .unwrap();
            let found = parse(
                linked.path(),
                "MIX_TEST_PARTITION=1 mix test --partitions 2",
            )
            .unwrap();
            assert!(found.files().is_none());
        }

        let no_mix = tempfile::tempdir().unwrap();
        let found = parse(
            no_mix.path(),
            "MIX_TEST_PARTITION=1 mix test --partitions 2",
        )
        .unwrap();
        assert!(found.files().is_none());
    }
}
