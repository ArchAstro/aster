//! Lays a model repo out as an Elixir project, one file per unit.
//!
//! Every definition ends in a revision number and every file starts with a
//! revision comment. An edit bumps the number on one line of the definition
//! and a comment edit bumps the comment, so a change touches exactly the lines
//! the model says it does.

use crate::repo::{Changes, Repo};
use std::collections::BTreeMap;

pub const PROJECT: &str = "shop";
const MIX_EXS: &str = "defmodule Shop.MixProject do\n  use Mix.Project\n  def project, do: [app: :shop, version: \"0.1.0\"]\nend\n";

/// Path of a unit's file, relative to the workspace root.
pub fn unit_path(repo: &Repo, unit: i64) -> String {
    if repo.test_units.contains(&unit) {
        format!("{PROJECT}/test/t{unit}_test.exs")
    } else {
        format!("{PROJECT}/lib/m{unit}.ex")
    }
}

/// Test file of a unit as `mix test` names it, relative to the project.
pub fn test_arg(unit: i64) -> String {
    format!("test/t{unit}_test.exs")
}

/// A path literal as written in source: `0` is an interpolated segment.
fn path_literal(segments: &[i64]) -> String {
    let mut out = String::from("\"");
    for segment in segments {
        out.push('/');
        if *segment == 0 {
            out.push_str("#{x}");
        } else {
            out.push_str(&format!("s{segment}"));
        }
    }
    out.push('"');
    out
}

/// A route pattern as the router declares it: `0` is `:id`.
fn route_pattern(segments: &[i64]) -> String {
    segments
        .iter()
        .map(|s| {
            if *s == 0 {
                "/:id".to_string()
            } else {
                format!("/s{s}")
            }
        })
        .collect()
}

/// The statements of one definition: every reference the model gives it.
fn items(repo: &Repo, def: i64) -> Vec<String> {
    let mut out = Vec::new();
    let name = |d: &i64| repo.name[d].as_str();
    for (_, to) in repo.calls.iter().filter(|(from, _)| *from == def) {
        out.push(format!("Shop.M{}.{}(x)", repo.unit[to], name(to)));
    }
    for (_, to) in repo.bare.iter().filter(|(from, _)| *from == def) {
        out.push(format!("{}(x)", name(to)));
    }
    for (_, unit) in repo.shapes.iter().filter(|(from, _)| *from == def) {
        out.push(format!("%Shop.M{unit}{{}}"));
    }
    for (_, path) in repo.requests.iter().filter(|(id, _)| *id == def) {
        out.push(path_literal(path));
    }
    if repo.reflects.contains(&def) {
        out.push("Application.spec(:shop, :modules)".to_string());
    }
    for (_, unit) in repo.preloads.iter().filter(|(from, _)| *from == def) {
        out.push(format!("Repo.preload(x, :assoc{unit})"));
    }
    for (_, route) in repo.built_paths.iter().filter(|(from, _)| *from == def) {
        // Joined at run time: no string literal holds the path.
        let mut parts = Vec::new();
        for segment in &repo.routes[route] {
            parts.push("<<47>>".to_string());
            parts.push(if *segment == 0 {
                "\"42\"".to_string()
            } else {
                format!("\"s{segment}\"")
            });
        }
        out.push(format!("Enum.join([{}])", parts.join(", ")));
    }
    for (_, tag) in repo.sends.iter().filter(|(id, _)| *id == def) {
        out.push(format!("GenServer.cast(x, {{:tag{tag}}})"));
    }
    if repo.sends_any.contains(&def) {
        out.push("GenServer.cast(x, x)".to_string());
    }
    for (_, to) in repo.computed.iter().filter(|(from, _)| *from == def) {
        out.push(format!(
            "apply(Module.concat([\"Shop\", \"M\" <> suffix]), :{}, [])",
            name(to)
        ));
    }
    out
}

fn render_unit(repo: &Repo, unit: i64, changes: &Changes) -> String {
    let test = repo.test_units.contains(&unit);
    let rev = |def: i64| i64::from(changes.edits.contains(&def));
    // Every code line that carries a revision also ends in a comment.
    let note = |def: i64| format!("# c{}", i64::from(changes.trailing.contains(&def)));
    let mut out = String::new();
    if test {
        out.push_str(&format!(
            "defmodule Shop.T{unit}Test do\n  use ExUnit.Case\n"
        ));
    } else {
        out.push_str(&format!("defmodule Shop.M{unit} do\n"));
    }
    out.push_str(&format!(
        "  # rev {}\n",
        i64::from(changes.comments.contains(&unit))
    ));
    for (_, to) in repo.imports.iter().filter(|(from, _)| *from == unit) {
        out.push_str(&format!("  use Shop.M{to}\n"));
    }
    for def in repo.defs_of(unit) {
        let kind = repo.kind[&def].as_str();
        let name = &repo.name[&def];
        let body = items(repo, def);
        match kind {
            "top" => {
                let mut list = vec![rev(def).to_string()];
                list.extend(body);
                out.push_str(&format!(
                    "  @top{unit} [{}] {}\n",
                    list.join(", "),
                    note(def)
                ));
            }
            "route" => {
                let action = repo.serves[&def];
                out.push_str(&format!(
                    "  get \"{}\", Shop.M{}, :{}, rev: {} {}\n",
                    route_pattern(&repo.routes[&def]),
                    repo.unit[&action],
                    repo.name[&action],
                    rev(def),
                    note(def)
                ));
            }
            "test" => {
                out.push_str(&format!("  test \"{name}\" do\n"));
                for item in body {
                    out.push_str(&format!("    {item}\n"));
                }
                out.push_str(&format!("    {} {}\n  end\n", rev(def), note(def)));
            }
            _ => {
                let definer = if kind == "macro" { "defmacro" } else { "def" };
                // A message clause matches a literal tag in its head.
                let head = match (repo.accepts.get(&def), name.as_str()) {
                    (Some(tag), "handle_call") => format!("{{:tag{tag}}}, _from, state"),
                    (Some(tag), _) => format!("{{:tag{tag}}}, state"),
                    (None, _) => "x".to_string(),
                };
                out.push_str(&format!("  {definer} {name}({head}) do\n"));
                for item in body {
                    out.push_str(&format!("    {item}\n"));
                }
                out.push_str(&format!("    {} {}\n  end\n", rev(def), note(def)));
            }
        }
    }
    out.push_str("end\n");
    out
}

/// Every file of the workspace, by path relative to its root.
pub fn files(repo: &Repo, changes: &Changes) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    files.insert("aster.toml".to_string(), String::new());
    files.insert(format!("{PROJECT}/mix.exs"), MIX_EXS.to_string());
    for unit in repo.units() {
        files.insert(unit_path(repo, unit), render_unit(repo, unit, changes));
    }
    files
}
