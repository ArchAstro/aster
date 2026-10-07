//! Command selection for `aster affected --only-affected-files` and
//! `--warnings-as-errors`.
//!
//! For each affected project the requested target, and any target in the
//! same project that it depends on, may declare the `files_list` capability.
//! Those targets run narrowed to the project's changed files: `{files}`
//! expands to the files, or the language plugin rewrites the command (Rust
//! follows module imports to related tests). A project with no changed files
//! of its own (selected only because a dependency changed), or whose
//! dependency also changed under `--dependents`, runs its targets in full,
//! because its own file list does not describe the change.

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::discovery::DiscoveredProject;
use crate::executor::CommandOverride;
use crate::plugins::related_tests::Delegated;
use crate::plugins::{
    FilesListPlan, FilesListSelection, PluginRegistry, RelatedTest, Target, TargetCapability,
};
use crate::related::{Outcome, Related};

/// What an affected run executes once files lists and warnings-as-errors are
/// applied.
#[derive(Debug, Default)]
pub struct AffectedCommands {
    /// Replacement commands by target address.
    pub overrides: HashMap<String, CommandOverride>,
    /// Primary projects that still run the requested target.
    pub primary: HashSet<String>,
    /// Requested targets dropped because none of their changed files is
    /// relevant.
    pub skipped: Vec<String>,
    /// How each target's command was chosen, by target address.
    pub notes: HashMap<String, Vec<String>>,
}

/// Inputs to [`plan_affected_commands`].
pub struct AffectedRequest<'a> {
    /// Requested target name.
    pub target: &'a str,
    /// Primary projects in execution order.
    pub projects: &'a [&'a DiscoveredProject],
    /// Changed files relative to the workspace root.
    pub changed_files: &'a [PathBuf],
    /// Addresses of the primary projects.
    pub primary: &'a HashSet<String>,
    /// Primary projects with a changed dependency.
    pub dependency_changed: &'a HashSet<String>,
    pub only_affected_files: bool,
    pub warnings_as_errors: bool,
    /// The source-level analysis under `--related`.
    pub related: Option<&'a Related>,
    /// Every discovered project, for targets that run in another project.
    pub all_projects: &'a [DiscoveredProject],
}

/// How a project's targets are narrowed.
enum Narrowing<'a> {
    /// Run as written, for this reason.
    Full(String),
    /// Narrow files-list targets to the project's own changed files.
    OwnFiles,
    /// Narrow test commands to the tests the change reaches.
    Related(&'a Outcome),
}

/// Choose the command overrides for an affected run.
pub fn plan_affected_commands(
    request: &AffectedRequest<'_>,
    registry: &PluginRegistry,
) -> Result<AffectedCommands> {
    let mut plan = AffectedCommands {
        primary: request.primary.clone(),
        ..AffectedCommands::default()
    };
    let target = request.target;

    // Under --related, targets that a project running in full depends on
    // are pinned before any other project's selection can narrow them.
    let planner = request.related.map(|related| {
        let mut planner = RelatedPlanner {
            lookup: Lookup::new(request.all_projects),
            related,
            registry,
            pinned: HashSet::new(),
        };
        let mut pinned = HashSet::new();
        for project in request.projects {
            let addr = address(project);
            let narrowed = related.projects.get(&addr).is_some_and(|outcome| {
                outcome.analysed
                    && (outcome.full.is_none() || planner.follows_runners(&addr, target, outcome))
            });
            if !narrowed {
                pinned.extend(
                    planner
                        .lookup
                        .closure(&addr, target)
                        .into_iter()
                        .skip(1)
                        .map(|(project, name)| format!("{project}:{name}")),
                );
            }
        }
        planner.pinned = pinned;
        planner
    });

    for project in request.projects {
        let project_addr = format!("//{}", project.relative_path.display());
        let target_addr = format!("{project_addr}:{target}");
        let Some(target_def) = project.targets.get(target) else {
            continue;
        };
        let mut requested: Option<Vec<String>> = None;

        let outcome = request
            .related
            .and_then(|related| related.projects.get(&project_addr));
        if request.only_affected_files || request.related.is_some() {
            let file_aware: Vec<String> = same_project_chain(project, &project_addr, target)
                .into_iter()
                .filter(|name| {
                    project
                        .targets
                        .get(name)
                        .is_some_and(|t| t.capabilities.contains(&TargetCapability::FilesList))
                })
                .collect();
            let project_files: Vec<PathBuf> = request
                .changed_files
                .iter()
                .filter(|f| f.starts_with(&project.relative_path))
                .map(|f| {
                    f.strip_prefix(&project.relative_path)
                        .unwrap_or(f)
                        .to_path_buf()
                })
                .collect();

            let narrowing = match outcome {
                Some(outcome) => match &outcome.full {
                    Some(_)
                        if planner.as_ref().is_some_and(|planner| {
                            planner.follows_runners(&project_addr, target, outcome)
                        }) =>
                    {
                        Narrowing::Related(outcome)
                    }
                    Some(reason) => Narrowing::Full(format!("{reason}; running in full")),
                    None if outcome.analysed => Narrowing::Related(outcome),
                    // Not analysed at source level: its plugin narrows it to
                    // its own changed files.
                    None => Narrowing::OwnFiles,
                },
                None if request.related.is_some() => {
                    Narrowing::Full("selected without a source analysis; running in full".into())
                }
                None if project_files.is_empty() => Narrowing::Full(
                    "no changed files of its own (selected as a dependent); running in full".into(),
                ),
                None if request.dependency_changed.contains(&project_addr) => {
                    Narrowing::Full("a project it depends on changed; running in full".into())
                }
                None => Narrowing::OwnFiles,
            };
            match narrowing {
                Narrowing::Full(reason) => {
                    if !file_aware.is_empty() || request.related.is_some() {
                        plan.note(&target_addr, reason.as_str());
                    }
                    for name in &file_aware {
                        if let Some(command) = full_command(&project.targets[name])? {
                            let addr = format!("{project_addr}:{name}");
                            plan.note(&addr, format!("command: {command}"));
                            if name == target {
                                requested = Some(vec![command]);
                            } else {
                                plan.overrides
                                    .insert(addr, CommandOverride::Run(vec![command]));
                            }
                        }
                    }
                }
                Narrowing::OwnFiles => {
                    for name in &file_aware {
                        let addr = format!("{project_addr}:{name}");
                        let selection = select_files_for_target(
                            &project.targets[name],
                            &project_files,
                            name,
                            &project.plugin_name,
                            registry,
                            &project.root,
                        )?;
                        for line in selection.explanation {
                            plan.note(&addr, line);
                        }
                        let is_requested = name == target;
                        match selection.plan {
                            FilesListPlan::Full => plan.note(&addr, "running in full"),
                            FilesListPlan::Commands(commands) => {
                                for command in &commands {
                                    plan.note(&addr, format!("command: {command}"));
                                }
                                if is_requested {
                                    requested = Some(commands);
                                } else {
                                    plan.overrides.insert(addr, CommandOverride::Run(commands));
                                }
                            }
                            FilesListPlan::Declined if !is_requested => {
                                plan.note(&addr, "the plugin did not narrow it; running in full");
                            }
                            FilesListPlan::Nothing | FilesListPlan::Declined if is_requested => {
                                plan.note(&addr, "skipped: no changed files are relevant to it");
                                plan.primary.remove(&project_addr);
                                plan.skipped.push(addr);
                                break;
                            }
                            FilesListPlan::Nothing | FilesListPlan::Declined => {
                                plan.note(&addr, "skipped: no changed files are relevant to it");
                                plan.overrides.insert(
                                    addr,
                                    CommandOverride::Skip(
                                        "no changed files are relevant to it (--only-affected-files)"
                                            .to_string(),
                                    ),
                                );
                            }
                        }
                    }
                }
                Narrowing::Related(outcome) => {
                    let skipped = match &planner {
                        Some(planner) => planner.plan_project(
                            &mut plan,
                            &project_addr,
                            target,
                            outcome,
                            &mut requested,
                        )?,
                        None => None,
                    };
                    if let Some(skip) = skipped {
                        plan.primary.remove(&project_addr);
                        plan.skipped.push(skip);
                    }
                }
            }
            if !plan.primary.contains(&project_addr) {
                continue;
            }
        }

        if request.warnings_as_errors
            && target_def
                .capabilities
                .contains(&TargetCapability::WarningsAsErrors)
        {
            let base = requested
                .clone()
                .unwrap_or_else(|| vec![target_def.command.clone()]);
            let mut modified = false;
            let commands: Vec<String> = base
                .iter()
                .map(|command| {
                    let temp = Target {
                        command: command.clone(),
                        ..target_def.clone()
                    };
                    match apply_warnings_as_errors(&temp, target, &project.plugin_name, registry) {
                        Some(changed) => {
                            modified = true;
                            changed
                        }
                        None => command.clone(),
                    }
                })
                .collect();
            if modified {
                requested = Some(commands);
            }
        }

        if let Some(commands) = requested {
            plan.overrides
                .insert(target_addr, CommandOverride::Run(commands));
        }
    }
    Ok(plan)
}

/// A narrowed command longer than this runs as written instead.
const MAX_COMMAND_BYTES: usize = 100_000;

/// Projects by address and by directory.
struct Lookup<'a> {
    by_addr: HashMap<String, &'a DiscoveredProject>,
    by_root: HashMap<PathBuf, &'a DiscoveredProject>,
}

impl<'a> Lookup<'a> {
    fn new(projects: &'a [DiscoveredProject]) -> Self {
        Self {
            by_addr: projects.iter().map(|p| (address(p), p)).collect(),
            by_root: projects.iter().map(|p| (p.root.clone(), p)).collect(),
        }
    }

    /// `target` of `project_addr` followed by every target it transitively
    /// depends on, in any project, as `(project address, target name)`.
    fn closure(&self, project_addr: &str, target: &str) -> Vec<(String, String)> {
        let start = (project_addr.to_string(), target.to_string());
        let mut seen: HashSet<(String, String)> = HashSet::from([start.clone()]);
        let mut order = vec![start.clone()];
        let mut queue = VecDeque::from([start]);
        while let Some((owner, name)) = queue.pop_front() {
            let Some(def) = self.by_addr.get(&owner).and_then(|p| p.targets.get(&name)) else {
                continue;
            };
            for dependency in &def.depends_on {
                let Some((project, name)) = dependency.rsplit_once(':') else {
                    continue;
                };
                let project = if project == "//self" { &owner } else { project };
                let next = (project.to_string(), name.to_string());
                if seen.insert(next.clone()) {
                    order.push(next.clone());
                    queue.push_back(next);
                }
            }
        }
        order
    }

    /// Where a target's command would run tests: in its own project, or in
    /// the project a shell one-liner changes into (a shard of that project).
    /// `None` when the one-liner changes into a directory that is not a
    /// project.
    fn runner(&self, owner: &'a DiscoveredProject, def: &Target) -> Option<Runner<'a>> {
        let Some(delegated) = Delegated::parse(&def.command) else {
            return Some(Runner {
                project: owner,
                command: def.command.clone(),
                delegated: None,
            });
        };
        let base = def.working_dir.as_deref().unwrap_or(&owner.root);
        let project = match &delegated.dir {
            Some(dir) => *self.by_root.get(&normalize(&base.join(dir)))?,
            None => owner,
        };
        Some(Runner {
            project,
            command: delegated.command.clone(),
            delegated: Some(delegated),
        })
    }
}

fn address(project: &DiscoveredProject) -> String {
    format!("//{}", project.relative_path.display())
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The test command behind a target and the project whose tests it runs.
struct Runner<'a> {
    project: &'a DiscoveredProject,
    command: String,
    delegated: Option<Delegated>,
}

/// What `--related` does with one target.
enum Narrowed<'a> {
    /// Not a test command Aster can narrow.
    NotRunner,
    /// A test command that must run everything, and why.
    AsWritten(String),
    /// Run these commands; the outcome explains the selection.
    Commands(Vec<String>, &'a Outcome),
    /// None of the tests it runs reaches the change.
    Nothing,
}

/// Shared inputs of a related plan.
struct RelatedPlanner<'a> {
    lookup: Lookup<'a>,
    related: &'a Related,
    registry: &'a PluginRegistry,
    /// Targets a project running in full depends on; they run as written
    /// whatever another project's selection says.
    pinned: HashSet<String>,
}

impl<'a> RelatedPlanner<'a> {
    fn tests(outcome: &Outcome) -> Vec<RelatedTest> {
        outcome
            .tests
            .iter()
            .map(|(file, selection)| RelatedTest {
                file: file.clone(),
                names: if selection.whole {
                    Vec::new()
                } else {
                    selection.names.iter().cloned().collect()
                },
            })
            .collect()
    }

    /// Narrow `def` of `owner` to the tests the change reaches in the
    /// project the command runs in.
    fn narrow(
        &self,
        owner: &'a DiscoveredProject,
        name: &str,
        def: &Target,
    ) -> Result<Narrowed<'a>> {
        let Some(runner) = self.lookup.runner(owner, def) else {
            return Ok(Narrowed::NotRunner);
        };
        let project = runner.project;
        let placeholder = runner.delegated.is_none()
            && def.command.contains("{files}")
            && def.capabilities.contains(&TargetCapability::FilesList);
        let plugin = self.registry.find_by_name(&project.plugin_name);
        let attempt = |tests: &[RelatedTest]| {
            plugin.and_then(|p| p.related_tests(&project.root, &runner.command, tests))
        };
        if !placeholder && attempt(&[]).is_none() {
            return Ok(Narrowed::NotRunner);
        }
        let Some(outcome) = self.related.projects.get(&address(project)) else {
            return Ok(Narrowed::AsWritten("its project was not analysed".into()));
        };
        if let Some(reason) = &outcome.full {
            return Ok(Narrowed::AsWritten(reason.clone()));
        }
        if !outcome.analysed {
            return Ok(Narrowed::AsWritten(format!(
                "{} is not analysed at source level",
                address(project)
            )));
        }
        let tests = Self::tests(outcome);
        let plan = if placeholder {
            let files: Vec<PathBuf> = tests.iter().map(|t| t.file.clone()).collect();
            select_files_for_target(
                def,
                &files,
                name,
                &project.plugin_name,
                self.registry,
                &project.root,
            )?
            .plan
        } else {
            attempt(&tests).unwrap_or(FilesListPlan::Full)
        };
        Ok(match plan {
            FilesListPlan::Commands(commands) => {
                let commands: Vec<String> = commands
                    .iter()
                    .map(|command| match &runner.delegated {
                        Some(delegated) => delegated.render(command),
                        None => command.clone(),
                    })
                    .collect();
                // Linux caps one argument at 128 KiB, and a shell script
                // passed with `-c` is one argument.
                if commands.iter().any(|c| c.len() > MAX_COMMAND_BYTES) {
                    return Ok(Narrowed::AsWritten(format!(
                        "{} selected test files do not fit on a command line",
                        tests.len()
                    )));
                }
                Narrowed::Commands(commands, outcome)
            }
            FilesListPlan::Nothing => Narrowed::Nothing,
            FilesListPlan::Full | FilesListPlan::Declined => {
                Narrowed::AsWritten("the command could not be narrowed".into())
            }
        })
    }

    /// Projects other than `project_addr` whose tests the target's
    /// dependency closure runs.
    fn runner_projects(&self, project_addr: &str, target: &str) -> HashSet<String> {
        let mut found = HashSet::new();
        for (owner_addr, name) in self.lookup.closure(project_addr, target) {
            let Some(owner) = self.lookup.by_addr.get(&owner_addr) else {
                continue;
            };
            let Some(def) = owner.targets.get(&name) else {
                continue;
            };
            let Some(runner) = self.lookup.runner(owner, def) else {
                continue;
            };
            let addr = address(runner.project);
            let narrowable = self
                .registry
                .find_by_name(&runner.project.plugin_name)
                .is_some_and(|p| {
                    p.related_tests(&runner.project.root, &runner.command, &[])
                        .is_some()
                });
            if narrowable && addr != project_addr {
                found.insert(addr);
            }
        }
        found
    }

    /// Whether a project that runs in full only because of dependencies can
    /// be narrowed after all: every such dependency is a project whose
    /// tests its targets run, so that project's selection is the whole
    /// story. This is a shard project.
    fn follows_runners(&self, project_addr: &str, target: &str, outcome: &Outcome) -> bool {
        if outcome.full_own || outcome.full_via.is_empty() {
            return false;
        }
        let runners = self.runner_projects(project_addr, target);
        outcome.full_via.iter().all(|via| runners.contains(via))
    }

    /// Narrow the requested target of one project and everything it depends
    /// on. Returns the requested target's address when nothing it runs
    /// reaches the change and the project should be dropped.
    fn plan_project(
        &self,
        plan: &mut AffectedCommands,
        project_addr: &str,
        target: &str,
        outcome: &Outcome,
        requested: &mut Option<Vec<String>>,
    ) -> Result<Option<String>> {
        let target_addr = format!("{project_addr}:{target}");
        let mut requested_is_runner = true;
        // Whether any test command in the closure still runs.
        let mut runs = false;
        let mut skips = 0;
        for (owner_addr, name) in self.lookup.closure(project_addr, target) {
            let Some(owner) = self.lookup.by_addr.get(&owner_addr).copied() else {
                continue;
            };
            let Some(def) = owner.targets.get(&name) else {
                continue;
            };
            let addr = format!("{owner_addr}:{name}");
            let is_requested = addr == target_addr;
            if !is_requested && self.pinned.contains(&addr) {
                runs = true;
                continue;
            }
            match self.narrow(owner, &name, def)? {
                Narrowed::NotRunner => {
                    if is_requested {
                        requested_is_runner = false;
                    }
                }
                Narrowed::AsWritten(reason) => {
                    runs = true;
                    plan.note(&addr, format!("{reason}; running as written"));
                }
                Narrowed::Commands(commands, selected) => {
                    runs = true;
                    if !plan.notes.contains_key(&addr) {
                        // Explain only the tests this command runs: a file
                        // it names, or a Go package it names.
                        let named = |file: &Path| {
                            let package = file
                                .parent()
                                .filter(|dir| !dir.as_os_str().is_empty())
                                .map_or(".".to_string(), |dir| format!("./{}", dir.display()));
                            let file = file.to_string_lossy();
                            commands.iter().any(|command| {
                                command.contains(file.as_ref())
                                    || command
                                        .split_whitespace()
                                        .any(|part| part.trim_matches('\'') == package)
                            })
                        };
                        for line in &selected.narrowed {
                            plan.note(&addr, line.as_str());
                        }
                        for (file, selection) in &selected.tests {
                            if !named(file) {
                                continue;
                            }
                            for reason in &selection.reasons {
                                plan.note(&addr, format!("{}: {reason}", file.display()));
                            }
                        }
                        for command in &commands {
                            plan.note(&addr, format!("command: {command}"));
                        }
                    }
                    if is_requested {
                        *requested = Some(commands);
                    } else {
                        plan.overrides.insert(addr, CommandOverride::Run(commands));
                    }
                }
                Narrowed::Nothing => {
                    skips += 1;
                    if !plan.notes.contains_key(&addr) {
                        plan.note(&addr, "skipped: no test it runs reaches the change");
                    }
                    if is_requested {
                        return Ok(Some(addr));
                    }
                    plan.overrides.insert(
                        addr,
                        CommandOverride::Skip(
                            "no test it runs reaches the change (--related)".into(),
                        ),
                    );
                }
            }
        }
        if !requested_is_runner {
            // A wrapper that does nothing itself exists to run the test
            // targets it depends on; with all of them skipped it has
            // nothing to do.
            let wrapper = self
                .lookup
                .by_addr
                .get(project_addr)
                .and_then(|p| p.targets.get(target))
                .is_some_and(|def| is_inert(&def.command));
            if !runs && skips > 0 && !outcome.changed && wrapper {
                plan.note(&target_addr, "skipped: no test it runs reaches the change");
                return Ok(Some(target_addr));
            }
            if !wrapper {
                plan.note(
                    &target_addr,
                    "not a test command Aster can narrow; running as written",
                );
            }
        }
        Ok(None)
    }
}

/// A command that does nothing: the body of a target that only groups the
/// targets it depends on.
fn is_inert(command: &str) -> bool {
    let command = command.trim();
    matches!(command, "true" | ":") || command.starts_with("echo ")
}

/// Projects that `--related` should run although nothing in them is
/// affected: a project whose requested target runs another project's tests
/// (a shard, or a wrapper that only groups test targets) follows that
/// project.
pub fn related_proxies(
    projects: &[DiscoveredProject],
    target: &str,
    related: &Related,
    registry: &PluginRegistry,
) -> HashSet<String> {
    let planner = RelatedPlanner {
        lookup: Lookup::new(projects),
        related,
        registry,
        pinned: HashSet::new(),
    };
    let needs_run = |addr: &String| {
        related
            .projects
            .get(addr)
            .is_some_and(|o| o.full.is_some() || !o.tests.is_empty())
    };
    projects
        .iter()
        .filter_map(|project| {
            let addr = address(project);
            let outcome = related.projects.get(&addr)?;
            let def = project.targets.get(target)?;
            if outcome.affected() {
                return None;
            }
            let delegates = planner
                .lookup
                .runner(project, def)
                .is_some_and(|runner| address(runner.project) != addr);
            // A wrapper in a project with code of its own is not a proxy:
            // its tests are that project's, and it runs when they are
            // affected.
            if !delegates && (outcome.has_sources || !is_inert(&def.command)) {
                return None;
            }
            planner
                .runner_projects(&addr, target)
                .iter()
                .any(needs_run)
                .then_some(addr)
        })
        .collect()
}

/// The command a files-list target runs with no file list: a standalone
/// `{files}` placeholder expands to nothing. `None` when the command has no
/// placeholder and runs as written.
fn full_command(target: &Target) -> Result<Option<String>> {
    if !target.command.contains("{files}") {
        return Ok(None);
    }
    let parts = shell_words::split(&target.command)
        .with_context(|| format!("invalid command quoting: {}", target.command))?;
    if parts
        .iter()
        .any(|part| part.contains("{files}") && part != "{files}")
    {
        anyhow::bail!(
            "{{files}} must be a standalone command argument, not embedded in a quoted or \
             combined argument: {}",
            target.command
        );
    }
    Ok(Some(
        parts
            .iter()
            .filter(|part| *part != "{files}")
            .map(|part| crate::executor::quote_command_argument(part))
            .collect::<Vec<_>>()
            .join(" "),
    ))
}

impl AffectedCommands {
    fn note(&mut self, addr: &str, line: impl Into<String>) {
        self.notes
            .entry(addr.to_string())
            .or_default()
            .push(line.into());
    }
}

/// `target` followed by every target of the same project it transitively
/// depends on.
fn same_project_chain(
    project: &DiscoveredProject,
    project_addr: &str,
    target: &str,
) -> Vec<String> {
    let prefix = format!("{project_addr}:");
    let mut chain = vec![target.to_string()];
    let mut seen: HashSet<String> = HashSet::from([target.to_string()]);
    let mut queue = VecDeque::from([target.to_string()]);
    while let Some(name) = queue.pop_front() {
        let Some(def) = project.targets.get(&name) else {
            continue;
        };
        for dependency in &def.depends_on {
            let Some(dep_name) = dependency
                .strip_prefix(&prefix)
                .or_else(|| dependency.strip_prefix("//self:"))
            else {
                continue;
            };
            if seen.insert(dep_name.to_string()) {
                chain.push(dep_name.to_string());
                queue.push_back(dep_name.to_string());
            }
        }
    }
    chain
}

/// Choose what a files-list target runs for `files` (relative to the
/// project): `{files}` expansion when the command has the placeholder,
/// otherwise the plugin's own selection.
pub fn select_files_for_target(
    target: &Target,
    files: &[PathBuf],
    target_name: &str,
    plugin_name: &str,
    registry: &PluginRegistry,
    project_dir: &Path,
) -> Result<FilesListSelection> {
    let nothing = || FilesListSelection::new(FilesListPlan::Nothing);
    if files.is_empty() {
        return Ok(nothing());
    }

    // Filter files by files_glob if specified
    let filtered_files: Vec<PathBuf> = if let Some(glob_pattern) = &target.files_glob {
        match Glob::new(glob_pattern) {
            Ok(glob) => {
                let matcher: GlobMatcher = glob.compile_matcher();
                files
                    .iter()
                    .filter(|f| {
                        // Match against filename only
                        f.file_name()
                            .map(|name| matcher.is_match(name))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            }
            Err(_) => files.to_vec(), // Invalid glob, use all files
        }
    } else {
        files.to_vec()
    };

    if filtered_files.is_empty() {
        return Ok(nothing());
    }

    // Expand only standalone argv placeholders. Textual substitution inside
    // an already quoted argument (especially `sh -c '... {files}'`) can turn a
    // filename into shell syntax after the outer command is parsed.
    if target.command.contains("{files}") {
        let parts = shell_words::split(&target.command)
            .with_context(|| format!("invalid command quoting: {}", target.command))?;
        if parts
            .iter()
            .any(|part| part.contains("{files}") && part != "{files}")
        {
            anyhow::bail!(
                "{{files}} must be a standalone command argument, not embedded in a quoted or \
                 combined argument: {}",
                target.command
            );
        }
        let program_index = parts
            .iter()
            .position(|part| !is_environment_assignment(part))
            .context("command contains only environment assignments")?;
        let placeholder_positions = parts
            .iter()
            .enumerate()
            .filter_map(|(index, part)| (part == "{files}").then_some(index))
            .collect::<Vec<_>>();
        if placeholder_positions.len() != 1 {
            anyhow::bail!("command must contain exactly one {{files}} placeholder");
        }
        let placeholder_index = placeholder_positions[0];
        if placeholder_index == program_index {
            anyhow::bail!("{{files}} cannot be used as the executable");
        }
        if parts[program_index..placeholder_index]
            .iter()
            .any(|part| is_command_interpreter_token(part))
        {
            anyhow::bail!(
                "{{files}} cannot be expanded directly through a command interpreter; \
                 use a fixed wrapper executable instead"
            );
        }

        let mut expanded = Vec::new();
        for part in parts {
            if part == "{files}" {
                expanded.extend(
                    filtered_files
                        .iter()
                        .map(|file| file.to_string_lossy().into_owned()),
                );
            } else {
                expanded.push(part);
            }
        }
        let command = expanded
            .iter()
            .map(|part| crate::executor::quote_command_argument(part))
            .collect::<Vec<_>>()
            .join(" ");
        return Ok(FilesListSelection::new(FilesListPlan::Commands(vec![
            command,
        ])));
    }

    // Fall back to the plugin for language-specific handling
    if let Some(plugin) = registry.find_by_name(plugin_name) {
        return Ok(plugin.select_for_files(
            project_dir,
            target_name,
            &target.command,
            &filtered_files,
        ));
    }

    Ok(nothing())
}

fn is_environment_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_command_interpreter_token(value: &str) -> bool {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "ksh"
            | "fish"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "env"
            | "sudo"
            | "xargs"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "ruby"
            | "perl"
            | "php"
            | "lua"
    ) || name.starts_with("python")
        || name.starts_with("pypy")
        || name.starts_with("ruby")
}

/// Apply warnings-as-errors to a command
///
/// Returns Some(modified_command) if the target supports warnings-as-errors,
/// None otherwise.
pub fn apply_warnings_as_errors(
    target: &Target,
    target_name: &str,
    plugin_name: &str,
    registry: &PluginRegistry,
) -> Option<String> {
    // Check if target has WarningsAsErrors capability
    if !target
        .capabilities
        .contains(&TargetCapability::WarningsAsErrors)
    {
        return None;
    }

    // Use plugin's with_warnings_as_errors for language-specific handling
    if let Some(plugin) = registry.find_by_name(plugin_name) {
        return plugin.with_warnings_as_errors(target_name, &target.command);
    }

    None
}

#[cfg(test)]
mod file_command_tests {
    use super::*;

    fn apply_files_to_command(
        target: &Target,
        files: &[PathBuf],
        target_name: &str,
        plugin_name: &str,
        registry: &PluginRegistry,
    ) -> Result<Option<String>> {
        let selection = select_files_for_target(
            target,
            files,
            target_name,
            plugin_name,
            registry,
            Path::new("."),
        )?;
        Ok(match selection.plan {
            FilesListPlan::Commands(mut commands) => commands.pop(),
            _ => None,
        })
    }

    fn files_target(command: &str) -> Target {
        Target {
            command: command.to_string(),
            depends_on: vec![],
            capabilities: HashSet::from([TargetCapability::FilesList]),
            files_glob: None,
            stream: false,
            cache: None,
            invalidates_cache: false,
            working_dir: None,
            exclusive_resources: vec![],
        }
    }

    #[test]
    fn files_placeholder_expands_to_literal_arguments() {
        let file = PathBuf::from("tests/x;touch${IFS}pwned.js");
        let expanded = apply_files_to_command(
            &files_target("./capture {files}"),
            std::slice::from_ref(&file),
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            shell_words::split(&expanded).unwrap(),
            vec!["./capture".to_string(), file.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn files_placeholder_inside_shell_script_is_rejected() {
        let error = apply_files_to_command(
            &files_target("sh -c 'tool {files}'"),
            &[PathBuf::from("test.js")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("{files} must be a standalone command argument"));
    }

    #[test]
    fn files_placeholder_as_executable_is_rejected() {
        let error = apply_files_to_command(
            &files_target("{files} --flag"),
            &[PathBuf::from("tool")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be used as the executable"));
    }

    #[test]
    fn files_placeholder_as_interpreter_input_is_rejected() {
        let error = apply_files_to_command(
            &files_target("sh -c {files}"),
            &[PathBuf::from("touch pwned")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be expanded directly through a command interpreter"));
    }

    #[test]
    fn files_placeholder_cannot_hide_an_interpreter_behind_a_launcher() {
        let error = apply_files_to_command(
            &files_target("nice -n 5 /bin/sh -c {files}"),
            &[PathBuf::from("touch pwned")],
            "test",
            "unknown",
            &PluginRegistry::new(),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be expanded directly through a command interpreter"));
    }
}
