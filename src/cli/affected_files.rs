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

use crate::config::RelatedWrapperConfig;
use crate::discovery::DiscoveredProject;
use crate::executor::CommandOverride;
use crate::plugins::related_tests::{package_script_runners, Delegated, Wrappers};
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
    /// Primary projects that answer for a changed dependency: under
    /// `--dependents`, those with one.
    pub dependency_changed: &'a HashSet<String>,
    pub only_affected_files: bool,
    pub warnings_as_errors: bool,
    /// The source-level analysis under `--related`.
    pub related: Option<&'a Related>,
    /// Every discovered project, for targets that run in another project.
    pub all_projects: &'a [DiscoveredProject],
    /// Projects selected because a changed file is a declared workspace
    /// input of the target. Their change is not in their own sources, so
    /// they run in full.
    pub declared_inputs: &'a HashSet<String>,
    /// `[affected.related] wrappers`: programs in front of a test command.
    pub related_wrappers: &'a [RelatedWrapperConfig],
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
            lookup: Lookup::new(request.all_projects, request.related_wrappers),
            related,
            registry,
            pinned: HashSet::new(),
        };
        let mut pinned = HashSet::new();
        for project in request.projects {
            let addr = address(project);
            let narrowed = !request.declared_inputs.contains(&addr)
                && related.projects.get(&addr).is_some_and(|outcome| {
                    outcome.analysed
                        && (outcome.full.is_none()
                            || planner.follows_runners(&addr, target, outcome))
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
                _ if request.declared_inputs.contains(&project_addr) => Narrowing::Full(
                    "a changed file is a declared workspace input; running in full".into(),
                ),
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
                None if request.dependency_changed.contains(&project_addr) => Narrowing::Full(
                    "a project it depends on changed (--dependents); running in full".into(),
                ),
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
    /// Targets that join a target's runs (`selected_by`), by its address.
    joiners: HashMap<String, Vec<(String, String)>>,
    wrappers: Wrappers,
}

impl<'a> Lookup<'a> {
    fn new(projects: &'a [DiscoveredProject], wrappers: &[RelatedWrapperConfig]) -> Self {
        let mut joiners: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for project in projects {
            for (name, def) in &project.targets {
                for selector in &def.selected_by {
                    joiners
                        .entry(selector.clone())
                        .or_default()
                        .push((address(project), name.clone()));
                }
            }
        }
        Self {
            by_addr: projects.iter().map(|p| (address(p), p)).collect(),
            by_root: projects.iter().map(|p| (p.root.clone(), p)).collect(),
            joiners,
            wrappers: Wrappers::new(wrappers),
        }
    }

    /// Makes running an `alias` target run the target it aliases too: a
    /// project keeps the alias as a copy of the aliased command, so the
    /// relation is read back from its `aster.toml`.
    fn join_aliases(&mut self, projects: &[DiscoveredProject]) {
        for project in projects {
            let Some(path) = crate::config::find_aster_toml(&project.root) else {
                continue;
            };
            let Ok(config) = crate::config::parse_aster_toml(&path) else {
                continue;
            };
            for (name, target) in &config.targets {
                if let crate::config::TargetConfig::Alias(alias) = target {
                    let aliased = alias.alias.strip_prefix("//self:").unwrap_or(&alias.alias);
                    self.joiners
                        .entry(format!("{}:{name}", address(project)))
                        .or_default()
                        .push((address(project), aliased.to_string()));
                }
            }
        }
    }

    /// `target` of `project_addr` followed by every target it transitively
    /// depends on, in any project, as `(project address, target name)`.
    /// Targets `selected_by` one of them join it, as the run does.
    fn closure(&self, project_addr: &str, target: &str) -> Vec<(String, String)> {
        let start = (project_addr.to_string(), target.to_string());
        let mut seen: HashSet<(String, String)> = HashSet::from([start.clone()]);
        let mut order = vec![start.clone()];
        let mut queue = VecDeque::from([start]);
        while let Some((owner, name)) = queue.pop_front() {
            let Some(def) = self.by_addr.get(&owner).and_then(|p| p.targets.get(&name)) else {
                continue;
            };
            let joining = self
                .joiners
                .get(&format!("{owner}:{name}"))
                .into_iter()
                .flatten()
                .cloned();
            let dependencies = def.depends_on.iter().filter_map(|dependency| {
                let (project, name) = dependency.rsplit_once(':')?;
                let project = if project == "//self" { &owner } else { project };
                Some((project.to_string(), name.to_string()))
            });
            for next in dependencies.collect::<Vec<_>>().into_iter().chain(joining) {
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
        // A configured wrapper program stays in front of the command it runs.
        let (wrapper, written) = match self.wrappers.strip(&def.command) {
            Some(wrapped) => (Some(wrapped.prefix), wrapped.command),
            None => (None, def.command.clone()),
        };
        let Some(delegated) = Delegated::parse(&written) else {
            return Some(Runner {
                project: owner,
                command: written,
                delegated: None,
                wrapper,
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
            wrapper,
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
    /// The wrapper program(s) written before `command`, with a trailing
    /// space.
    wrapper: Option<String>,
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
        // The plugin sees the environment a one-liner sets for the command,
        // which the one-liner itself keeps.
        let environment = runner
            .delegated
            .as_ref()
            .map(|delegated| delegated.environment())
            .unwrap_or_default();
        let command = format!("{environment}{}", runner.command);
        let hidden_environment = runner
            .delegated
            .as_ref()
            .is_some_and(Delegated::hides_environment);
        let attempt = |tests: &[RelatedTest]| {
            plugin.and_then(|p| {
                if hidden_environment {
                    p.related_tests_in_hidden_environment(&project.root, &command, tests)
                } else {
                    p.related_tests(&project.root, &command, tests)
                }
            })
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
                        Some(delegated) => {
                            delegated.render(command.strip_prefix(&environment).unwrap_or(command))
                        }
                        None => command.clone(),
                    })
                    .map(|command| match (&runner.wrapper, placeholder) {
                        (Some(wrapper), false) => format!("{wrapper}{command}"),
                        _ => command,
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
        // A selected test the project's runner cannot name is run by some
        // command here that Aster cannot narrow, so nothing is narrowed or
        // skipped on the strength of the tests it can.
        if !outcome.outside.is_empty() {
            for (file, selection) in &outcome.outside {
                for reason in &selection.reasons {
                    plan.note(&target_addr, format!("{}: {reason}", file.display()));
                }
            }
            plan.note(
                &target_addr,
                "a selected test is not in the project's own language; running as written",
            );
            return Ok(None);
        }
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
    wrappers: &[RelatedWrapperConfig],
) -> HashSet<String> {
    let planner = RelatedPlanner {
        lookup: Lookup::new(projects, wrappers),
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

/// A target whose command runs tests, for `aster check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestTarget {
    /// Address of the project that defines the target.
    pub project: String,
    pub target: String,
    pub command: String,
    /// Why `--related` runs the whole target whenever it selects it. `None`
    /// when the command is narrowed to the selected tests.
    pub unnarrowable: Option<String>,
    /// Address of the project whose tests the command runs (another
    /// project's, for a shard that changes into it).
    pub runs_in: String,
    /// The runner, as the words before its first flag or path
    /// (`mix test`, `vitest`). A launcher in front (`pnpm exec`, `npx`) is
    /// left out.
    pub program: String,
    /// The `--config` the command passes, if any.
    pub config: Option<String>,
    /// The command picks tests by tag or name (`--include`, `--only`), so
    /// another run of the runner does not cover it. A `--config` is in
    /// [`Self::config`] instead.
    pub selective: bool,
    /// The whole-suite run this command makes as `(program, config)`: itself
    /// when it names no test file.
    pub whole_suites: Vec<(String, Option<String>)>,
}

impl TestTarget {
    /// The command runs every crate of its Cargo workspace (`--workspace`,
    /// `--all`).
    pub fn whole_suite_flags(&self) -> bool {
        self.command
            .split_whitespace()
            .any(|w| w == "--workspace" || w == "--all")
    }

    /// A `cargo test` or `cargo nextest run` command.
    pub fn is_cargo_test(&self) -> bool {
        matches!(self.program.as_str(), "cargo test" | "cargo nextest run")
    }

    /// The command picks packages, targets or tests (`-p`, `--test`, a name
    /// filter after `--`), so it does not run a whole crate.
    pub fn selects_cargo_tests(&self) -> bool {
        self.command.split_whitespace().any(|w| {
            matches!(
                w,
                "-p" | "--package"
                    | "--exclude"
                    | "--test"
                    | "--lib"
                    | "--bin"
                    | "--bins"
                    | "--example"
                    | "--bench"
                    | "--doc"
                    | "-E"
                    | "--filterset"
                    | "--"
            ) || w.starts_with("--package=")
                || w.starts_with("--test=")
                || w.starts_with("--filterset=")
        })
    }
}

/// Why a command with no test runner in its words is still reported, when
/// it is: a package script that runs a runner Aster cannot narrow, or, for an
/// entry target, any command that is not inert.
fn hidden_runner_reason(runner: Option<&Runner>, entry: bool) -> Option<String> {
    use crate::plugins::related_tests::{package_script_body, package_script_runners};
    let Some(runner) = runner else {
        return entry.then(|| {
            "its shell script changes into a directory that is not a project".to_string()
        });
    };
    let root = &runner.project.root;
    if let Some((name, body)) = package_script_body(root, &runner.command) {
        if let Some(found) = package_script_runners(root, &runner.command).first() {
            return Some(format!(
                "package script `{name}` contains `{found}`, which Aster cannot narrow (it \
                 narrows `npm` and `pnpm` scripts whose `&&` chain it can read; a `cd`, a pipe, \
                 an interpreter script, a `pre`/`post` hook or another launcher stops it)"
            ));
        }
        return entry.then(|| {
            format!(
                "package script `{name}` (`{body}`) contains no test runner Aster can narrow; \
                 put the runner command in the target"
            )
        });
    }
    if !entry {
        return None;
    }
    let words = shell_words::split(&runner.command).unwrap_or_default();
    let words: Vec<&String> = words
        .iter()
        .filter(|w| !is_environment_assignment(w))
        .collect();
    if let Some(file) = script_file(&words) {
        return Some(format!(
            "it runs the script file `{file}`, which Aster does not read; put the runner command \
             in the target"
        ));
    }
    Some(format!(
        "`{}` is not a test runner Aster can narrow",
        runner.command
    ))
}

/// The script file a command runs: `./test-ci.sh`, `bash tools/run.sh`,
/// `node scripts/test.mjs`.
fn script_file(words: &[&String]) -> Option<String> {
    let first = words.first()?.as_str();
    let name = first.rsplit('/').next().unwrap_or(first);
    let interpreters = [
        "sh", "bash", "zsh", "dash", "node", "python", "python3", "ruby", "perl", "tsx", "deno",
    ];
    if interpreters.contains(&name) {
        return words
            .iter()
            .skip(1)
            .take_while(|w| w.as_str() != "-c")
            .find(|w| !w.starts_with('-'))
            .map(|w| w.to_string());
    }
    (first.contains('/') || name.contains('.')).then(|| first.to_string())
}

/// How a test command selects what it runs.
struct RunnerShape {
    program: String,
    config: Option<String>,
    /// Names no test file.
    whole: bool,
    selective: bool,
}

/// The runner of `command`, whether it lists test files, its `--config`, and
/// whether it picks tests by tag or name.
fn describe_runner(command: &str) -> RunnerShape {
    let words = shell_words::split(command).unwrap_or_default();
    let words: Vec<&str> = words
        .iter()
        .filter(|w| !is_environment_assignment(w))
        .map(String::as_str)
        .collect();
    let plain = |w: &str| !w.starts_with('-') && !w.contains(['/', '.', '=']);
    // The first word is the program even when it is a path
    // (`.venv/bin/python -m pytest`); its arguments are not.
    let head = words
        .first()
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .filter(|w| !w.starts_with('-') && !w.contains(['.', '=']) && !w.is_empty());
    let mut program: Vec<&str> = match (head, words.get(1), words.get(2)) {
        (Some(head), Some(&"-m"), Some(module)) if head.starts_with("python") => {
            vec![head, "-m", module]
        }
        (Some(head), ..) => std::iter::once(head)
            .chain(words[1..].iter().copied().take_while(|w| plain(w)))
            .take(4)
            .collect(),
        (None, ..) => Vec::new(),
    };
    // The same runner whichever launcher starts it, and with or without
    // `run` (`vitest` and `vitest run` both run the suite once in CI).
    match program.as_slice() {
        ["npx" | "bunx", ..] => {
            program.remove(0);
        }
        ["pnpm" | "yarn" | "npm" | "bun", "exec" | "x" | "dlx", ..] => {
            program.drain(..2);
        }
        ["pnpm" | "yarn", "vitest" | "jest" | "mocha", ..] => {
            program.remove(0);
        }
        _ => {}
    }
    if program.first() == Some(&"vitest") {
        program.truncate(1);
    }
    // `python -m pytest`: `-m` there names a module, not a marker.
    let consumed = if program.get(1) == Some(&"-m") { 3 } else { 1 };
    let mut config = None;
    // The value of `--config` is not a test path.
    let mut rest = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if matches!(w, "--config" | "-c") {
            config = words.get(i + 1).map(|v| v.to_string());
            i += 1;
        } else if let Some(value) = w.strip_prefix("--config=") {
            config = Some(value.to_string());
        } else {
            rest.push(w);
        }
        i += 1;
    }
    let path_like = |w: &str| {
        // `./...` is every package: the suite, not a path.
        !w.starts_with('-')
            && !matches!(w, "./..." | "...")
            && (w.contains('/')
                || [
                    ".exs", ".ex", ".ts", ".tsx", ".js", ".mjs", ".py", ".go", ".rs",
                ]
                .iter()
                .any(|ext| w.ends_with(ext)))
    };
    let selective = rest.iter().skip(consumed).any(|w| {
        matches!(
            *w,
            "--include"
                | "--only"
                | "--exclude"
                | "--tag"
                | "-t"
                | "--project"
                | "--grep"
                | "-g"
                | "-k"
                | "-m"
                | "--testNamePattern"
        )
    });
    RunnerShape {
        program: program.join(" "),
        config,
        whole: !rest.iter().skip(consumed).any(|w| path_like(w)),
        selective,
    }
}

/// The whole-suite run of a `{files}` command with an empty list, as
/// `(program, config)`: the runner it names, behind a script
/// (`related-tests.mjs vitest {files}`) or a `--run=<path>` option.
fn files_runner(command: &str) -> Option<(String, Option<String>)> {
    let words = shell_words::split(&command.replace("{files}", "")).ok()?;
    words.iter().skip(1).find_map(|word| {
        let word = word.strip_prefix("--run=").unwrap_or(word);
        let name = word.rsplit('/').next().unwrap_or(word);
        let program = match name {
            "vitest" => "vitest",
            "jest" => "jest",
            "mocha" => "mocha",
            "pytest" => "pytest",
            "go" => "go test",
            "cargo" => "cargo test",
            "bun" => "bun test",
            "node" => "node",
            _ => return None,
        };
        Some((program.to_string(), None))
    })
}

/// A target whose command runs a whole suite of a runner other than the
/// command's own words, for `aster check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteRun {
    pub project: String,
    pub target: String,
    /// Address of the project whose tests it runs.
    pub runs_in: String,
    /// `(program, config)` of each whole-suite run.
    pub suites: Vec<(String, Option<String>)>,
}

/// The targets that run a whole suite through something `describe_runner`
/// cannot see: a package script (`pnpm test` where `test` is `vitest run`),
/// or a `{files}` command, which runs the whole suite of the runner it names
/// when the list is empty. Used only to decide what a reached target covers;
/// it does not decide which targets are test targets.
pub fn suite_runs(
    projects: &[DiscoveredProject],
    wrappers: &[RelatedWrapperConfig],
) -> Vec<SuiteRun> {
    let lookup = Lookup::new(projects, wrappers);
    let mut found = Vec::new();
    for project in projects {
        for (name, def) in &project.targets {
            let Some(runner) = lookup.runner(project, def) else {
                continue;
            };
            let suites: Vec<(String, Option<String>)> = if def.command.contains("{files}")
                && def.capabilities.contains(&TargetCapability::FilesList)
            {
                files_runner(&def.command).into_iter().collect()
            } else {
                package_script_runners(&runner.project.root, &runner.command)
                    .iter()
                    .map(|command| describe_runner(command))
                    .filter(|s| s.whole && !s.selective)
                    .map(|s| (s.program, s.config))
                    .collect()
            };
            if !suites.is_empty() {
                found.push(SuiteRun {
                    project: address(project),
                    target: name.clone(),
                    runs_in: address(runner.project),
                    suites,
                });
            }
        }
    }
    found
}

/// The targets whose command runs tests, ordered by address.
///
/// A command the project's plugin reads as a test runner (through the
/// configured wrappers and a shell one-liner's final command, as
/// `--related` reads it) is one. So is a command that contains such a runner
/// among other commands, an interpreter's script or a pipeline: `--related`
/// cannot narrow it, and the reason is the one narrowing computes.
///
/// A target named in `entries` (a CI entry such as `test-ci`) is a test target
/// whatever its command is, unless the command is inert: an entry exists to
/// run tests, so a command Aster cannot read as a runner (a script file, a
/// package script with no runner it can narrow) is one it cannot narrow, and
/// `--related` runs it as written.
pub fn test_targets(
    projects: &[DiscoveredProject],
    wrappers: &[RelatedWrapperConfig],
    registry: &PluginRegistry,
    entries: &[String],
) -> Vec<TestTarget> {
    use crate::plugins::related_tests::{refusal, simple_commands, trailing_commands};
    let lookup = Lookup::new(projects, wrappers);
    let mut found = Vec::new();
    for project in projects {
        let mut names: Vec<&String> = project.targets.keys().collect();
        names.sort();
        for name in names {
            let def = &project.targets[name];
            // Watch mode runs on a developer's machine, not in CI.
            if name == "watch"
                || name.ends_with(":watch")
                || name.ends_with("-watch")
                || name.ends_with("_watch")
            {
                continue;
            }
            let written = match lookup.wrappers.strip(&def.command) {
                Some(wrapped) => wrapped.command,
                None => def.command.clone(),
            };
            let runner = lookup.runner(project, def);
            // The same attempt `--related` makes when it narrows the target.
            let narrowable = runner.as_ref().is_some_and(|runner| {
                let environment = runner
                    .delegated
                    .as_ref()
                    .map(Delegated::environment)
                    .unwrap_or_default();
                let command = format!("{environment}{}", runner.command);
                let hidden = runner
                    .delegated
                    .as_ref()
                    .is_some_and(Delegated::hides_environment);
                registry
                    .find_by_name(&runner.project.plugin_name)
                    .is_some_and(|plugin| {
                        if hidden {
                            plugin.related_tests_in_hidden_environment(
                                &runner.project.root,
                                &command,
                                &[],
                            )
                        } else {
                            plugin.related_tests(&runner.project.root, &command, &[])
                        }
                        .is_some()
                    })
            });
            let placeholder = def.command.contains("{files}")
                && def.capabilities.contains(&TargetCapability::FilesList);
            // The runner is not in the command's words, only behind a script.
            let mut hidden = false;
            let unnarrowable = if narrowable || placeholder {
                None
            } else {
                // A test runner somewhere inside the command makes it a
                // test target that cannot be narrowed.
                let reads = |command: &str| {
                    registry
                        .plugins()
                        .iter()
                        .find(|p| p.related_tests(&project.root, command, &[]).is_some())
                        .map(|p| p.name().to_string())
                };
                let segments = simple_commands(&written);
                let whole = segments
                    .iter()
                    .find_map(|command| reads(command).map(|plugin| (plugin, None)));
                // Behind a program Aster does not know runs a command.
                let behind = || {
                    segments.iter().find_map(|command| {
                        trailing_commands(command)
                            .into_iter()
                            .find_map(|(prefix, rest)| {
                                reads(&rest).map(|plugin| (plugin, Some(prefix)))
                            })
                    })
                };
                match whole.or_else(behind) {
                    None => {
                        // No runner in the written words: a script file, or a
                        // package script whose runners Aster cannot narrow.
                        // Only a target known to run tests is reported.
                        let entry = entries.contains(name) && !is_inert(&def.command);
                        match hidden_runner_reason(runner.as_ref(), entry) {
                            Some(reason) => {
                                hidden = true;
                                Some(reason)
                            }
                            None => continue,
                        }
                    }
                    Some((recogniser, wrapper)) => {
                        Some(match (&runner, refusal(&written), wrapper) {
                            (None, _, _) => {
                                "its shell script changes into a directory that is not a project"
                                    .to_string()
                            }
                            (_, Some(reason), _) => reason.to_string(),
                            (_, None, Some(prefix)) => format!(
                        "it starts with `{prefix}`, which is not a configured wrapper; if that \
                         program runs the command after it, list it in \
                         `[affected.related] wrappers`"
                    ),
                            _ if recogniser != project.plugin_name => format!(
                        "it runs a {recogniser} test command in a {} project, and a command is \
                         narrowed only by the plugin of the project that runs it",
                        project.plugin_name
                    ),
                            _ => "its form is not one the test runner's narrowing supports \
                          (an unsupported flag or argument)"
                                .to_string(),
                        })
                    }
                }
            };
            if placeholder && !narrowable {
                // A `{files}` placeholder is filled from the selection; it
                // is a test target only if the plugin recognises the rest.
                let stripped = written.replace("{files}", "");
                let recognised = registry.plugins().iter().any(|p| {
                    p.related_tests(&project.root, stripped.trim(), &[])
                        .is_some()
                });
                if !recognised {
                    continue;
                }
            }
            let (runs_in, runner_command) = match &runner {
                Some(runner) => (address(runner.project), runner.command.as_str()),
                None => (address(project), written.as_str()),
            };
            let shape = describe_runner(runner_command);
            // A script Aster cannot read runs no suite it can name.
            let whole_suites = Some(&shape)
                .filter(|s| s.whole && !s.selective && !hidden)
                .map(|s| (s.program.clone(), s.config.clone()))
                .into_iter()
                .collect();
            found.push(TestTarget {
                project: address(project),
                target: name.clone(),
                command: def.command.clone(),
                unnarrowable,
                runs_in,
                program: shape.program,
                config: shape.config,
                selective: shape.selective,
                whole_suites,
            });
        }
    }
    found.sort_by(|a, b| (&a.project, &a.target).cmp(&(&b.project, &b.target)));
    found
}

/// Every `(project address, target)` that running the `roots` runs: the
/// roots, what they depend on and what is `selected_by` any of those.
pub fn reachable_targets(
    projects: &[DiscoveredProject],
    wrappers: &[RelatedWrapperConfig],
    roots: &[(String, String)],
) -> HashSet<(String, String)> {
    let mut lookup = Lookup::new(projects, wrappers);
    lookup.join_aliases(projects);
    roots
        .iter()
        .flat_map(|(project, target)| lookup.closure(project, target))
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
            selected_by: vec![],
            retry_setup: None,
            report: None,
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

#[cfg(test)]
mod describe_runner_tests {
    use super::describe_runner;

    #[test]
    fn a_path_to_the_interpreter_is_still_the_program() {
        let whole = describe_runner(".venv/bin/python -m pytest");
        assert_eq!(whole.program, "python -m pytest");
        assert!(whole.whole && !whole.selective);
        let subset = describe_runner(
            ".venv/bin/python -m pytest tests/test_cli.py tests/test_archastro_provider.py",
        );
        assert_eq!(subset.program, whole.program);
        assert!(!subset.whole);
    }

    #[test]
    fn go_test_all_packages_is_the_suite_and_a_package_list_is_a_subset() {
        let whole = describe_runner("go test ./...");
        assert_eq!(whole.program, "go test");
        assert!(whole.whole);
        for subset in [
            "go test -count=1 ./internal/x",
            "go test -race ./cmd/gateway ./internal/gateway",
            "go test ./cmd/...",
        ] {
            let shape = describe_runner(subset);
            assert_eq!(shape.program, "go test", "{subset}");
            assert!(!shape.whole && !shape.selective, "{subset}");
        }
    }

    #[test]
    fn the_value_of_config_is_not_a_test_file() {
        for command in [
            "vitest run --coverage --config vitest.cov.config.ts",
            "vitest run -c vitest.cov.config.ts",
            "vitest run --config=vitest.cov.config.ts",
        ] {
            let shape = describe_runner(command);
            assert!(shape.whole, "{command}");
            assert_eq!(shape.config.as_deref(), Some("vitest.cov.config.ts"));
        }
    }

    #[test]
    fn the_launcher_is_not_part_of_the_runner() {
        for command in ["pnpm exec vitest run", "npx vitest", "pnpm vitest run"] {
            assert_eq!(describe_runner(command).program, "vitest", "{command}");
        }
    }
}
