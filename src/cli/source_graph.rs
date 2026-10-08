//! `aster graph --source`: the definitions a change reaches, printed as the
//! source graph `--related` selects tests from.

use crate::related::{Change, GraphNode, Related};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

/// Keep the changes under `dir` whose extension is one of `extensions`; an
/// empty filter keeps everything.
pub fn filter_changes(changes: &mut Vec<Change>, dir: Option<&str>, extensions: &[String]) {
    let dir = dir.map(|dir| Path::new(dir.trim_start_matches("./")));
    changes.retain(|change| {
        let under = dir.is_none_or(|dir| change.path.starts_with(dir));
        let extension = change.path.extension().and_then(|e| e.to_str());
        let wanted = extensions.is_empty()
            || extension.is_some_and(|e| extensions.iter().any(|x| x.trim_start_matches('.') == e));
        under && wanted
    });
}

/// How a range is named in output: the expression, or the working tree.
pub fn describe_range(range: Option<&str>) -> String {
    match range.map(str::trim).filter(|r| !r.is_empty()) {
        Some(range) if range.contains("..") => range.to_string(),
        Some(reference) => format!("the working tree against {reference}"),
        None => "the working tree".to_string(),
    }
}

fn label(node: &GraphNode) -> String {
    match &node.name {
        Some(name) => format!("{name} ({}:{})", node.file.display(), node.line),
        None => node.file.display().to_string(),
    }
}

/// Projects that run every test, with the reason, ordered by address.
fn full_runs(analysis: &Related) -> BTreeMap<&str, &str> {
    analysis
        .projects
        .iter()
        .filter_map(|(address, outcome)| Some((address.as_str(), outcome.full.as_deref()?)))
        .collect()
}

/// The graph as a forest: each changed definition, and beneath it what
/// uses it, down to the tests.
pub fn render_text(analysis: &Related, range: &str) -> String {
    let nodes = &analysis.graph;
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        match node.via {
            Some(parent) => children[parent].push(index),
            None => roots.push(index),
        }
    }
    let tests = nodes.iter().filter(|node| node.test).count();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Source graph for {range}: {} changed, {} reached, {tests} in test files",
        roots.len(),
        nodes.len() - roots.len(),
    );
    for &root in &roots {
        let node = &nodes[root];
        let reason = node.changed.as_deref().unwrap_or("changed");
        let _ = writeln!(out, "\n{}  [{reason}]", label(node));
        branch(&mut out, nodes, &children, root, "");
    }
    let full = full_runs(analysis);
    if !full.is_empty() {
        let _ = writeln!(out, "\nRun in full, beyond what the graph shows:");
        for (address, reason) in full {
            let _ = writeln!(out, "  {address}: {reason}");
        }
    }
    out
}

fn branch(out: &mut String, nodes: &[GraphNode], children: &[Vec<usize>], at: usize, indent: &str) {
    let below = &children[at];
    for (position, &child) in below.iter().enumerate() {
        let last = position + 1 == below.len();
        let node = &nodes[child];
        let mark = if node.test { "  [test]" } else { "" };
        let _ = writeln!(
            out,
            "{indent}{} {}{mark}",
            if last { "└─" } else { "├─" },
            label(node)
        );
        let deeper = format!("{indent}{}", if last { "   " } else { "│  " });
        branch(out, nodes, children, child, &deeper);
    }
}

/// The graph as nodes and edges; an edge runs from a definition to one
/// that uses it.
pub fn render_json(analysis: &Related, range: &str) -> Value {
    let edges: Vec<Value> = analysis
        .graph
        .iter()
        .enumerate()
        .filter_map(|(index, node)| Some(json!({ "from": node.via?, "to": index })))
        .collect();
    let nodes: Vec<Value> = analysis
        .graph
        .iter()
        .enumerate()
        .map(|(index, node)| {
            json!({
                "id": index,
                "file": node.file,
                "name": node.name,
                "kind": node.kind,
                "line": node.line,
                "project": node.project,
                "test": node.test,
                "changed": node.changed,
            })
        })
        .collect();
    let full: BTreeMap<&str, &str> = full_runs(analysis);
    json!({ "range": range, "nodes": nodes, "edges": edges, "full": full })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn node(file: &str, name: &str, line: usize, via: Option<usize>) -> GraphNode {
        GraphNode {
            file: PathBuf::from(file),
            name: Some(name.to_string()),
            kind: "function",
            line,
            project: None,
            test: file.contains("test"),
            changed: via.is_none().then(|| "lib/a.ex:3 changed".to_string()),
            via,
        }
    }

    #[test]
    fn text_nests_users_beneath_the_change() {
        let analysis = Related {
            graph: vec![
                node("lib/a.ex", "sum", 3, None),
                node("lib/b.ex", "total", 4, Some(0)),
                node("test/b_test.exs", "total", 5, Some(1)),
            ],
            ..Related::default()
        };
        let text = render_text(&analysis, "main..HEAD");
        assert_eq!(
            text,
            "Source graph for main..HEAD: 1 changed, 2 reached, 1 in test files\n\
             \nsum (lib/a.ex:3)  [lib/a.ex:3 changed]\n\
             └─ total (lib/b.ex:4)\n   └─ total (test/b_test.exs:5)  [test]\n"
        );
    }

    #[test]
    fn filters_keep_the_named_directory_and_extensions() {
        let change = |path: &str| Change {
            path: PathBuf::from(path),
            ..Change::default()
        };
        let mut changes = vec![change("app/a.ex"), change("app/b.ts"), change("lib/c.ex")];
        filter_changes(&mut changes, Some("./app"), &["ex".to_string()]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("app/a.ex"));
    }
}
