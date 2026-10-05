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
use crate::plugins::{FilesListPlan, FilesListSelection, PluginRegistry, Target, TargetCapability};

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
    /// Projects with a changed dependency (empty without `--dependents`).
    pub dependency_changed: &'a HashSet<String>,
    pub only_affected_files: bool,
    pub warnings_as_errors: bool,
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

    for project in request.projects {
        let project_addr = format!("//{}", project.relative_path.display());
        let target_addr = format!("{project_addr}:{target}");
        let Some(target_def) = project.targets.get(target) else {
            continue;
        };
        let mut requested: Option<Vec<String>> = None;

        if request.only_affected_files {
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

            let full_reason = if project_files.is_empty() {
                Some("no changed files of its own (selected as a dependent); running in full")
            } else if request.dependency_changed.contains(&project_addr) {
                Some("a project it depends on changed; running in full")
            } else {
                None
            };
            if file_aware.is_empty() {
                // Nothing to narrow.
            } else if let Some(reason) = full_reason {
                plan.note(&target_addr, reason);
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
            } else {
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
                        FilesListPlan::Nothing if is_requested => {
                            plan.note(&addr, "skipped: no changed files are relevant to it");
                            plan.primary.remove(&project_addr);
                            plan.skipped.push(addr);
                            break;
                        }
                        FilesListPlan::Nothing => {
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
