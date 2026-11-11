//! Stored `aster affected` plans: `--plan-out` writes the plan of every lane
//! a planning run produced, `--plan-in` executes one lane from it without
//! repeating the analysis.
//!
//! A CI pipeline plans once (`affected --matrix --plan-out plan.json`) and
//! each job replays its own lane (`affected --lane X --plan-in plan.json`).
//! A stored plan is only used when the [`PlanKey`] it was written under equals
//! the key of the replaying invocation. Any difference, and any problem
//! reading the file, makes the caller analyse as if `--plan-in` was absent:
//! a replay never runs a selection that analysis would not have chosen.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::affected_files::AffectedCommands;
use crate::discovery::DiscoveredProject;
use crate::executor::CommandOverride;

/// Version of the file layout. A file with another value is not replayed.
pub const SCHEMA: u32 = 1;

/// Everything the selection depends on besides the lane. Two invocations
/// with equal keys, on the same commit, choose the same targets and commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanKey {
    /// Aster that wrote the plan; its planning rules may differ in another.
    pub aster_version: String,
    /// Commit `--base` resolved to (`-` under `--all`).
    pub base_sha: String,
    /// Commit `--head` (default `HEAD`) resolved to (`-` under `--all`).
    pub head_sha: String,
    /// `--head` was given. Without it uncommitted changes are part of the
    /// plan, and `changed_files_digest` is what pins them.
    pub head_pinned: bool,
    /// `all`, `related`, `dependents` or `changed`.
    pub mode: String,
    /// The requested target (a lane may run another one).
    pub target: String,
    pub dependents: bool,
    pub only_affected_files: bool,
    pub warnings_as_errors: bool,
    pub lang: Vec<String>,
    /// sha256 of the sorted changed files after `affected.ignore`, with each
    /// file's content on disk.
    pub changed_files_digest: String,
    /// sha256 of the uncommitted and untracked files with their content, so
    /// a dirty tree never matches a plan, with or without `--head`.
    pub dirty_digest: String,
    /// sha256 of every project address with its targets (command and
    /// dependencies), its config file's content and the root `aster.toml`,
    /// which hold the lanes.
    pub projects_digest: String,
}

impl PlanKey {
    /// The first field that differs from `current`, worded for a log line.
    fn mismatch(&self, current: &PlanKey) -> Option<String> {
        fn differ<T: PartialEq + std::fmt::Debug>(
            name: &str,
            stored: &T,
            current: &T,
        ) -> Option<String> {
            (stored != current)
                .then(|| format!("{name} differs (plan has {stored:?}, this run has {current:?})"))
        }
        differ("aster version", &self.aster_version, &current.aster_version)
            .or_else(|| differ("base", &self.base_sha, &current.base_sha))
            .or_else(|| differ("head", &self.head_sha, &current.head_sha))
            .or_else(|| differ("--head", &self.head_pinned, &current.head_pinned))
            .or_else(|| differ("mode", &self.mode, &current.mode))
            .or_else(|| differ("target", &self.target, &current.target))
            .or_else(|| differ("--dependents", &self.dependents, &current.dependents))
            .or_else(|| {
                differ(
                    "--only-affected-files",
                    &self.only_affected_files,
                    &current.only_affected_files,
                )
            })
            .or_else(|| {
                differ(
                    "--warnings-as-errors",
                    &self.warnings_as_errors,
                    &current.warnings_as_errors,
                )
            })
            .or_else(|| differ("--lang", &self.lang, &current.lang))
            .or_else(|| {
                differ(
                    "changed files",
                    &self.changed_files_digest,
                    &current.changed_files_digest,
                )
            })
            .or_else(|| differ("dirty files", &self.dirty_digest, &current.dirty_digest))
            .or_else(|| {
                differ(
                    "discovered projects",
                    &self.projects_digest,
                    &current.projects_digest,
                )
            })
    }
}

/// What the invocation was asked, for [`PlanKey::new`].
pub struct KeyInputs<'a> {
    pub base_sha: String,
    pub head_sha: String,
    pub head_pinned: bool,
    pub all: bool,
    pub related: bool,
    pub dependents: bool,
    pub only_affected_files: bool,
    pub warnings_as_errors: bool,
    pub target: &'a str,
    pub lang: &'a [String],
    pub changed_files: &'a HashSet<PathBuf>,
    /// Uncommitted and untracked files.
    pub dirty_files: &'a HashSet<PathBuf>,
    pub projects: &'a [DiscoveredProject],
    /// Workspace root, where the changed files are read from.
    pub root: &'a Path,
}

impl PlanKey {
    pub fn new(inputs: KeyInputs<'_>) -> PlanKey {
        let mode = if inputs.all {
            "all"
        } else if inputs.related {
            "related"
        } else if inputs.dependents {
            "dependents"
        } else {
            "changed"
        };
        let with_content = |files: &HashSet<PathBuf>| -> Vec<String> {
            let mut lines: Vec<String> = files
                .iter()
                .map(|path| {
                    let name = path.to_string_lossy().into_owned();
                    format!("{name}\0{}", file_digest(&inputs.root.join(path)))
                })
                .collect();
            lines.sort();
            lines
        };
        let mut changed = with_content(inputs.changed_files);
        let dirty = with_content(inputs.dirty_files);
        changed.sort();
        let mut projects: Vec<String> = inputs
            .projects
            .iter()
            .map(|project| {
                let mut targets: Vec<&str> = project.targets.keys().map(String::as_str).collect();
                targets.sort();
                let mut detail = String::new();
                for name in &targets {
                    let target = &project.targets[*name];
                    let mut depends_on = target.depends_on.clone();
                    depends_on.sort();
                    detail.push_str(&format!("{name}={:?}<{depends_on:?};", target.command));
                }
                format!(
                    "//{}:{detail}:{}:{}",
                    project.relative_path.display(),
                    file_digest(&project.config_path),
                    file_digest(&inputs.root.join(&project.relative_path).join("aster.toml")),
                )
            })
            .collect();
        projects.sort();
        projects.push(format!(
            "root:{}",
            file_digest(&inputs.root.join("aster.toml"))
        ));
        let mut lang = inputs.lang.to_vec();
        lang.sort();
        PlanKey {
            aster_version: env!("CARGO_PKG_VERSION").to_string(),
            base_sha: inputs.base_sha,
            head_sha: inputs.head_sha,
            head_pinned: inputs.head_pinned,
            mode: mode.to_string(),
            target: inputs.target.to_string(),
            dependents: inputs.dependents,
            only_affected_files: inputs.only_affected_files,
            warnings_as_errors: inputs.warnings_as_errors,
            lang,
            changed_files_digest: digest_lines(&changed),
            dirty_digest: digest_lines(&dirty),
            projects_digest: digest_lines(&projects),
        }
    }
}

/// sha256 of a file's content, or a marker when it cannot be read.
fn file_digest(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => crate::cache::Digest::of_bytes(&bytes).as_str().to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "absent".to_string(),
        Err(_) => "unreadable".to_string(),
    }
}

fn digest_lines(lines: &[String]) -> String {
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(0);
    }
    crate::cache::Digest::of_bytes(&bytes).as_str().to_string()
}

/// A replacement command as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredOverride {
    Run(Vec<String>),
    Skip(String),
}

/// One lane's plan: the dry-run JSON for display, and the selection the
/// executor needs to run it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredLane {
    /// The target the lane runs (its own, or the requested one).
    pub target: String,
    /// Projects selected before command planning, sorted.
    pub affected: Vec<String>,
    /// Selected projects were non-empty but none defines the target.
    pub missing_among_affected: bool,
    /// Whether command planning ran (`--related`, `--only-affected-files`,
    /// `--warnings-as-errors`). Without it the executor takes `affected`.
    pub has_command_plan: bool,
    /// Projects that run the requested target after command planning, sorted.
    pub primary: Vec<String>,
    pub overrides: BTreeMap<String, StoredOverride>,
    pub skipped: Vec<String>,
    pub notes: BTreeMap<String, Vec<String>>,
    /// The lane's `--dry-run --json` document.
    pub plan: DryRunOutput,
}

impl StoredLane {
    pub fn new(
        target: &str,
        affected: &HashSet<String>,
        missing_among_affected: bool,
        commands: Option<&AffectedCommands>,
        plan: DryRunOutput,
    ) -> StoredLane {
        let sorted = |set: &HashSet<String>| {
            let mut list: Vec<String> = set.iter().cloned().collect();
            list.sort();
            list
        };
        StoredLane {
            target: target.to_string(),
            affected: sorted(affected),
            missing_among_affected,
            has_command_plan: commands.is_some(),
            primary: commands.map(|c| sorted(&c.primary)).unwrap_or_default(),
            overrides: commands
                .map(|c| {
                    c.overrides
                        .iter()
                        .map(|(address, over)| {
                            let stored = match over {
                                CommandOverride::Run(commands) => {
                                    StoredOverride::Run(commands.clone())
                                }
                                CommandOverride::Skip(reason) => {
                                    StoredOverride::Skip(reason.clone())
                                }
                            };
                            (address.clone(), stored)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            skipped: commands.map(|c| c.skipped.clone()).unwrap_or_default(),
            notes: commands
                .map(|c| {
                    c.notes
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            plan,
        }
    }

    pub fn affected_set(&self) -> HashSet<String> {
        self.affected.iter().cloned().collect()
    }

    /// The command plan the lane was written from, when it had one.
    pub fn commands(&self) -> Option<AffectedCommands> {
        self.has_command_plan.then(|| AffectedCommands {
            overrides: self
                .overrides
                .iter()
                .map(|(address, stored)| {
                    let over = match stored {
                        StoredOverride::Run(commands) => CommandOverride::Run(commands.clone()),
                        StoredOverride::Skip(reason) => CommandOverride::Skip(reason.clone()),
                    };
                    (address.clone(), over)
                })
                .collect::<HashMap<_, _>>(),
            primary: self.primary.iter().cloned().collect(),
            skipped: self.skipped.clone(),
            notes: self
                .notes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
    }
}

/// One target of a dry-run plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DryRunTarget {
    pub address: String,
    pub reason: String,
    pub files: Vec<String>,
    /// Commands replacing the target's own command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// How the target's command was chosen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selection: Vec<String>,
}

/// The `--dry-run --json` document of one lane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DryRunOutput {
    pub target: String,
    pub base: String,
    pub head: Option<String>,
    pub targets: Vec<DryRunTarget>,
    pub count: usize,
    /// Requested targets dropped by --only-affected-files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Plugins of every project the plan runs a target in, plus the lane's
    /// declared toolchains: what a runner must install.
    pub toolchains: Vec<String>,
}

impl DryRunOutput {
    /// A plan with nothing to run.
    pub fn empty(target: &str, base: &str, head: Option<&str>) -> DryRunOutput {
        DryRunOutput {
            target: target.to_string(),
            base: base.to_string(),
            head: head.map(str::to_string),
            targets: Vec::new(),
            count: 0,
            skipped: Vec::new(),
            toolchains: Vec::new(),
        }
    }
}

/// The file `--plan-out` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanFile {
    pub schema: u32,
    pub key: PlanKey,
    /// Lanes by name; the key of the unnamed lane is the empty string.
    pub lanes: BTreeMap<String, StoredLane>,
}

/// Collects lanes while a planning run goes, then writes the file.
pub struct PlanRecorder {
    path: PathBuf,
    key: PlanKey,
    lanes: BTreeMap<String, StoredLane>,
}

impl PlanRecorder {
    pub fn new(path: &Path, key: PlanKey) -> PlanRecorder {
        PlanRecorder {
            path: path.to_path_buf(),
            key,
            lanes: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, lane: &str, stored: StoredLane) {
        self.lanes.insert(lane.to_string(), stored);
    }

    /// Write the file through a temporary sibling so a reader never sees half
    /// a plan.
    pub fn write(&self) -> Result<()> {
        let file = PlanFile {
            schema: SCHEMA,
            key: self.key.clone(),
            lanes: self.lanes.clone(),
        };
        let mut text = serde_json::to_string_pretty(&file)?;
        text.push('\n');
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        std::fs::write(&temporary, text)
            .with_context(|| format!("Failed to write {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("Failed to write {}", self.path.display()))
    }
}

/// Read `path` and check it against this invocation. `Err` carries the one
/// line reason the plan cannot be used; the caller logs it and analyses.
///
/// `lanes` are the requested lanes with the target each resolves to now.
pub fn load(
    path: &Path,
    current: &PlanKey,
    lanes: &[(String, String)],
    projects: &[DiscoveredProject],
) -> std::result::Result<PlanFile, String> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "file does not exist".to_string()
        } else {
            format!("cannot read it ({error})")
        }
    })?;
    check(&text, current, lanes, projects)
}

fn check(
    text: &str,
    current: &PlanKey,
    lanes: &[(String, String)],
    projects: &[DiscoveredProject],
) -> std::result::Result<PlanFile, String> {
    let file: PlanFile =
        serde_json::from_str(text).map_err(|error| format!("cannot parse it ({error})"))?;
    if file.schema != SCHEMA {
        return Err(format!(
            "schema {} is not the supported schema {SCHEMA}",
            file.schema
        ));
    }
    if let Some(reason) = file.key.mismatch(current) {
        return Err(reason);
    }
    let known: HashSet<String> = projects
        .iter()
        .map(|project| format!("//{}", project.relative_path.display()))
        .collect();
    for (name, target) in lanes {
        let Some(stored) = file.lanes.get(name) else {
            let have: Vec<&str> = file.lanes.keys().map(String::as_str).collect();
            return Err(format!(
                "lane '{name}' is not in the plan (it has: {})",
                have.join(", ")
            ));
        };
        if &stored.target != target {
            return Err(format!(
                "lane '{name}' runs '{}' in the plan and '{target}' now",
                stored.target
            ));
        }
        if let Some(unknown) = stored
            .affected
            .iter()
            .chain(&stored.primary)
            .find(|address| !known.contains(*address))
        {
            return Err(format!("lane '{name}' names {unknown}, which is not here"));
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PlanKey {
        PlanKey {
            aster_version: env!("CARGO_PKG_VERSION").to_string(),
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            head_pinned: false,
            mode: "related".into(),
            target: "test".into(),
            dependents: false,
            only_affected_files: false,
            warnings_as_errors: false,
            lang: vec![],
            changed_files_digest: "c".into(),
            dirty_digest: "e".into(),
            projects_digest: "d".into(),
        }
    }

    fn commands() -> AffectedCommands {
        AffectedCommands {
            overrides: HashMap::from([
                (
                    "//a:test".to_string(),
                    CommandOverride::Run(vec!["go test -run '^(TestX)$' ./...".into()]),
                ),
                (
                    "//b:test".to_string(),
                    CommandOverride::Skip("no affected files".into()),
                ),
            ]),
            primary: HashSet::from(["//a".to_string()]),
            skipped: vec!["//b:test".into()],
            notes: HashMap::from([("//a:test".to_string(), vec!["narrowed".to_string()])]),
        }
    }

    fn file_text(key: &PlanKey) -> String {
        let affected = HashSet::from(["//a".to_string(), "//b".to_string()]);
        let stored = StoredLane::new(
            "test",
            &affected,
            false,
            Some(&commands()),
            DryRunOutput::empty("test", "main", None),
        );
        serde_json::to_string(&PlanFile {
            schema: SCHEMA,
            key: key.clone(),
            lanes: BTreeMap::from([("platform".to_string(), stored)]),
        })
        .unwrap()
    }

    fn lanes(name: &str) -> Vec<(String, String)> {
        vec![(name.to_string(), "test".to_string())]
    }

    #[test]
    fn a_lane_gives_back_the_command_plan_it_was_written_from() {
        let original = commands();
        let affected = HashSet::from(["//a".to_string()]);
        let stored = StoredLane::new(
            "test",
            &affected,
            false,
            Some(&original),
            DryRunOutput::empty("test", "main", None),
        );
        let text = serde_json::to_string(&stored).unwrap();
        let back: StoredLane = serde_json::from_str(&text).unwrap();
        let restored = back.commands().unwrap();
        assert_eq!(restored.overrides, original.overrides);
        assert_eq!(restored.primary, original.primary);
        assert_eq!(restored.skipped, original.skipped);
        assert_eq!(restored.notes, original.notes);
        assert_eq!(back.affected_set(), affected);
    }

    #[test]
    fn a_lane_without_command_planning_stays_without() {
        let stored = StoredLane::new(
            "test",
            &HashSet::new(),
            false,
            None,
            DryRunOutput::empty("test", "main", None),
        );
        assert!(stored.commands().is_none());
    }

    #[test]
    fn a_plan_naming_projects_that_are_not_here_is_refused() {
        let k = key();
        let file = check(&file_text(&k), &k, &lanes("platform"), &[]);
        // The projects //a and //b are not discovered in this unit test.
        assert!(file.unwrap_err().contains("not here"));
    }

    #[test]
    fn every_key_field_that_differs_is_named() {
        let stored = key();
        let mut other = key();
        other.base_sha = "f".repeat(40);
        assert!(stored.mismatch(&other).unwrap().starts_with("base differs"));
        let mut other = key();
        other.head_sha = "f".repeat(40);
        assert!(stored.mismatch(&other).unwrap().starts_with("head differs"));
        let mut other = key();
        other.mode = "dependents".into();
        assert!(stored.mismatch(&other).unwrap().starts_with("mode differs"));
        let mut other = key();
        other.aster_version = "0.0.0".into();
        assert!(stored
            .mismatch(&other)
            .unwrap()
            .starts_with("aster version differs"));
        let mut other = key();
        other.changed_files_digest = "x".into();
        assert!(stored
            .mismatch(&other)
            .unwrap()
            .starts_with("changed files differs"));
        assert!(stored.mismatch(&key()).is_none());
    }

    #[test]
    fn unreadable_content_and_foreign_schemas_are_refused() {
        let k = key();
        assert!(check("not json", &k, &lanes("platform"), &[])
            .unwrap_err()
            .contains("cannot parse"));
        let wrong = file_text(&k).replacen("\"schema\":1", "\"schema\":99", 1);
        assert!(check(&wrong, &k, &lanes("platform"), &[])
            .unwrap_err()
            .contains("schema 99"));
    }

    #[test]
    fn a_lane_missing_from_the_plan_is_refused_with_the_lanes_it_has() {
        let k = key();
        let error = check(&file_text(&k), &k, &lanes("tooling"), &[]).unwrap_err();
        assert!(
            error.contains("lane 'tooling' is not in the plan"),
            "{error}"
        );
        assert!(error.contains("platform"), "{error}");
    }

    #[test]
    fn a_lane_that_runs_another_target_now_is_refused() {
        let k = key();
        let requested = vec![("platform".to_string(), "test-ci".to_string())];
        let error = check(&file_text(&k), &k, &requested, &[]).unwrap_err();
        assert!(error.contains("runs 'test' in the plan"), "{error}");
    }

    #[test]
    fn a_missing_file_is_reported_as_missing() {
        let error = load(Path::new("/nonexistent/plan.json"), &key(), &[], &[]).unwrap_err();
        assert_eq!(error, "file does not exist");
    }

    #[test]
    fn the_recorder_writes_a_file_the_loader_reads_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested/plan.json");
        let mut recorder = PlanRecorder::new(&path, key());
        recorder.record(
            "platform",
            StoredLane::new(
                "test",
                &HashSet::new(),
                false,
                None,
                DryRunOutput::empty("test", "main", None),
            ),
        );
        recorder.write().unwrap();
        let file = load(&path, &key(), &lanes("platform"), &[]).unwrap();
        assert_eq!(file.key, key());
        assert!(file.lanes.contains_key("platform"));
        assert!(!dir.path().join("nested/plan.json.tmp").exists());
    }
}
