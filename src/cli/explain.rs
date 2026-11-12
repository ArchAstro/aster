//! `aster affected --explain`: why each project, lane and test is selected.
//!
//! The explanation of a project says which rule put it in the run: it owns a
//! changed file, it depends on one that does, a changed file is a declared
//! input of the target, it runs the tests of a project that is selected, or
//! (under `--related`) the source graph reaches some of its tests. For those
//! tests it carries the shortest chain of graph edges from a changed
//! definition to the test, one hop per definition, each saying how the
//! definition before it reaches it. The chains come from the same walk that
//! selects the tests, in every language the source graph reads.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::graph::ProjectGraph;
use crate::related::{Hop, Outcome, Related};

/// Tests listed per project unless `--explain-limit` says otherwise.
pub const DEFAULT_TEST_LIMIT: usize = 20;

/// Why one project is selected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectWhy {
    pub project: String,
    /// The rules that selected it, in plain words.
    pub reasons: Vec<String>,
    /// How many test files the source graph selects in it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub selected_tests: usize,
    /// The selected test files with the shortest chains, up to the limit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestWhy>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Why one test file is selected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestWhy {
    /// Relative to the workspace root.
    pub file: String,
    /// The whole file runs, not only the tests named.
    pub whole: bool,
    /// The tests selected in the file, when not the whole file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// From the changed definition (first) to the test (last). Empty when
    /// the file is selected for a reason that is not an edge of the graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<HopOut>,
}

/// One hop of a chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HopOut {
    /// The definition, or the file for code outside every definition.
    pub node: String,
    pub file: String,
    pub line: usize,
    /// For the first hop, what changed. For the others, how the hop before
    /// it reaches this one.
    pub edge: String,
}

impl From<&Hop> for HopOut {
    fn from(hop: &Hop) -> Self {
        HopOut {
            node: hop.node.clone(),
            file: hop.file.display().to_string(),
            line: hop.line,
            edge: hop.edge.clone(),
        }
    }
}

/// What the explanation of a project is built from.
pub struct Context<'a> {
    pub related: Option<&'a Related>,
    pub graph: &'a ProjectGraph,
    /// Projects that own a changed file.
    pub directly_affected: &'a HashSet<String>,
    /// Changed files per project, workspace-relative.
    pub files_per_project: &'a HashMap<String, Vec<String>>,
    /// Projects selected because a changed file is a declared input of the
    /// target.
    pub declared_inputs: &'a HashSet<String>,
    pub limit: usize,
}

/// Explain why `project` runs the requested target.
pub fn explain_project(
    project: &str,
    relative_path: &std::path::Path,
    ctx: &Context,
) -> ProjectWhy {
    let mut reasons = Vec::new();
    if let Some(files) = ctx.files_per_project.get(project) {
        reasons.push(format!(
            "owns {} changed file{}: {}",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            list(files)
        ));
    }
    if ctx.declared_inputs.contains(project) {
        reasons.push("a changed file is a declared workspace input of the target".to_string());
    }
    let mut tests = Vec::new();
    let mut selected = 0;
    let outcome = ctx
        .related
        .and_then(|related| related.projects.get(project));
    match outcome {
        Some(outcome) => {
            reasons.extend(outcome_reasons(outcome));
            let mut all: Vec<TestWhy> = outcome
                .tests
                .iter()
                .chain(&outcome.outside)
                .map(|(file, selection)| TestWhy {
                    file: relative_path.join(file).display().to_string(),
                    whole: selection.whole,
                    names: selection.names.iter().cloned().collect(),
                    chain: selection.chain.iter().map(HopOut::from).collect(),
                })
                .collect();
            selected = all.len();
            if selected > 0 {
                reasons.push(format!(
                    "the source graph reaches {selected} test file{} from the change",
                    if selected == 1 { "" } else { "s" }
                ));
            }
            // Shortest chains first: they are the ones to read.
            all.sort_by(|a, b| (a.chain.len(), &a.file).cmp(&(b.chain.len(), &b.file)));
            if ctx.limit > 0 {
                all.truncate(ctx.limit);
            }
            tests = all;
            if reasons.is_empty() && !outcome.affected() {
                reasons.push(
                    "it runs the tests of another selected project (a shard or wrapper)"
                        .to_string(),
                );
            }
        }
        None if ctx.related.is_some() => {
            reasons.push("selected without a source analysis".to_string());
        }
        None => {}
    }
    if reasons.is_empty() {
        match path_to_changed(project, ctx) {
            Some(path) => reasons.push(format!(
                "depends on a project that owns a changed file: {}",
                path.join(" -> ")
            )),
            None => reasons.push("selected by the lane or the target's own rules".to_string()),
        }
    }
    ProjectWhy {
        project: project.to_string(),
        reasons,
        selected_tests: selected,
        tests,
    }
}

fn outcome_reasons(outcome: &Outcome) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Some(full) = &outcome.full {
        reasons.push(format!("runs in full: {full}"));
    }
    for line in &outcome.narrowed {
        reasons.push(line.clone());
    }
    for (target, why) in &outcome.changed_targets {
        reasons.push(format!("target `{target}` changed: {why}"));
    }
    reasons
}

fn list(files: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut text = files
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > SHOWN {
        let _ = write!(text, ", and {} more", files.len() - SHOWN);
    }
    text
}

/// The shortest chain of declared dependencies from `project` to a project
/// that owns a changed file, `project` first.
fn path_to_changed(project: &str, ctx: &Context) -> Option<Vec<String>> {
    let mut parent: BTreeMap<String, String> = BTreeMap::new();
    let mut queue = VecDeque::from([project.to_string()]);
    let mut seen: HashSet<String> = HashSet::from([project.to_string()]);
    while let Some(next) = queue.pop_front() {
        if next != project && ctx.directly_affected.contains(&next) {
            let mut path = vec![next.clone()];
            let mut at = next;
            while let Some(previous) = parent.get(&at) {
                path.push(previous.clone());
                at = previous.clone();
            }
            path.reverse();
            return Some(path);
        }
        for dependency in ctx.graph.dependencies(&next) {
            if seen.insert(dependency.address.clone()) {
                parent.insert(dependency.address.clone(), next.clone());
                queue.push_back(dependency.address.clone());
            }
        }
    }
    None
}

/// The explanation as lines of text, each to be indented by the caller.
pub fn render_lines(why: &ProjectWhy) -> Vec<String> {
    let mut lines = vec!["why selected:".to_string()];
    for reason in &why.reasons {
        lines.push(format!("  - {reason}"));
    }
    for test in &why.tests {
        let detail = if test.whole {
            "whole file".to_string()
        } else {
            format!("{} test(s)", test.names.len())
        };
        if test.chain.is_empty() {
            lines.push(format!("  {} ({detail})", test.file));
            continue;
        }
        lines.push(format!(
            "  {} ({detail}, {} hop{})",
            test.file,
            test.chain.len() - 1,
            if test.chain.len() == 2 { "" } else { "s" }
        ));
        for (position, hop) in test.chain.iter().enumerate() {
            let place = if hop.line == 0 {
                hop.file.clone()
            } else {
                format!("{}:{}", hop.file, hop.line)
            };
            let name = if hop.line == 0 {
                String::new()
            } else {
                format!("{} ", hop.node)
            };
            if position == 0 {
                lines.push(format!(
                    "    {position}. {name}({place})  changed: {}",
                    hop.edge
                ));
            } else if hop.edge.is_empty() {
                lines.push(format!("    {position}. {name}({place})"));
            } else {
                lines.push(format!("    {position}. {name}({place})  <- {}", hop.edge));
            }
        }
    }
    let hidden = why.selected_tests.saturating_sub(why.tests.len());
    if hidden > 0 {
        lines.push(format!(
            "  and {hidden} more selected test files (--explain-limit 0 lists them all)"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(node: &str, line: usize, edge: &str) -> HopOut {
        HopOut {
            node: node.to_string(),
            file: "lib/a.ex".to_string(),
            line,
            edge: edge.to_string(),
        }
    }

    #[test]
    fn a_chain_reads_change_first_and_names_each_edge() {
        let why = ProjectWhy {
            project: "//app".to_string(),
            reasons: vec!["owns 1 changed file: lib/a.ex".to_string()],
            selected_tests: 3,
            tests: vec![TestWhy {
                file: "test/a_test.exs".to_string(),
                whole: true,
                names: vec![],
                chain: vec![
                    hop("label", 3, "lib/a.ex:3 changed"),
                    hop("run", 9, "refers to `label`"),
                ],
            }],
        };
        let text = render_lines(&why).join("\n");
        assert!(
            text.contains("test/a_test.exs (whole file, 1 hop)"),
            "{text}"
        );
        assert!(
            text.contains("0. label (lib/a.ex:3)  changed: lib/a.ex:3 changed"),
            "{text}"
        );
        assert!(
            text.contains("1. run (lib/a.ex:9)  <- refers to `label`"),
            "{text}"
        );
        assert!(text.contains("and 2 more selected test files"), "{text}");
    }
}
