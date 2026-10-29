//! `aster:` directives in comments, which correct what the string-literal
//! analysis concludes about a definition.
//!
//! - `aster: not-a-read` applies to the line it trails, or to the next line
//!   of code when the comment stands alone. String literals starting on
//!   that line are not recorded as naming workspace paths.
//! - `aster: reads <path> [<path>...]` declares paths the enclosing
//!   definition reads without a literal showing it. The definition is
//!   seeded by a changed file that matches, as a literal would seed it.
//!
//! Comments are found as syntax-tree nodes, so the directive works in every
//! language the analysis parses. The directive must start the comment text
//! (after the language's comment marker); anything after ` -- ` is a free
//! reason. An annotation changes what its definition names, so its lines
//! are classified as part of that definition instead of as inert comments.

use super::facts::{Def, FileFacts, LineClass};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tree_sitter::Tree;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Directive {
    NotARead,
    /// Workspace-relative paths or globs, normalised (no `//`, `./` or
    /// trailing `/`).
    Reads(Vec<String>),
    /// A directive name Aster does not know.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    /// The 1-based line of the comment that holds the directive.
    pub line: usize,
    pub directive: Directive,
    /// The line the directive applies to: for `not-a-read`, the code line
    /// whose literals it covers.
    pub target: usize,
    /// For `reads`, the index of the definition that declares the paths
    /// (`None`: code outside every definition).
    pub owner: Option<u32>,
    /// For `not-a-read`, the literals it removed from the definition.
    pub suppressed: Vec<String>,
}

/// One comment line that starts with a directive.
struct Found {
    line: usize,
    directive: Directive,
    trailing: bool,
    /// Last line of the comment node that holds it.
    comment_end: usize,
}

/// The text of a directive in one line of a comment, if it has one.
fn parse_line(line: &str) -> Option<Directive> {
    let text = line.trim();
    let text = text.trim_start_matches(['/', '*', '#', '!', ' ', '\t']);
    let text = text.trim_end().trim_end_matches("*/").trim_end();
    let rest = text.strip_prefix("aster:")?;
    let rest = match rest.find("--") {
        Some(at) if rest[..at].ends_with(char::is_whitespace) || at == 0 => &rest[..at],
        _ => rest,
    };
    let mut words = rest.split_whitespace();
    let name = words.next().unwrap_or("");
    Some(match name {
        "not-a-read" => Directive::NotARead,
        "reads" => Directive::Reads(words.filter_map(normalise).collect()),
        other => Directive::Unknown(other.to_string()),
    })
}

fn normalise(path: &str) -> Option<String> {
    let path = path.trim_matches(|c| c == ',' || c == '"' || c == '\'');
    let path = path.strip_prefix("//").unwrap_or(path);
    let path = path.trim_start_matches("./").trim_end_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

fn is_comment(kind: &str) -> bool {
    kind == "comment" || kind.ends_with("_comment")
}

/// Every comment of the tree, as `(first line, last line, trailing)`, and
/// the directives among their lines.
fn scan(tree: &Tree, source: &str) -> (Vec<Found>, HashSet<usize>) {
    let mut found = Vec::new();
    let mut comment_lines = HashSet::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if !is_comment(node.kind()) {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
            continue;
        }
        let first = node.start_position().row + 1;
        let mut last = node.end_position().row + 1;
        if node.end_position().column == 0 && last > first {
            last -= 1;
        }
        let before = &source[source[..node.start_byte()]
            .rfind('\n')
            .map_or(0, |at| at + 1)..node.start_byte()];
        let trailing = !before.trim().is_empty();
        if !trailing {
            comment_lines.extend(first..=last);
        }
        let text = &source[node.byte_range()];
        for (offset, line) in text.lines().enumerate() {
            if let Some(directive) = parse_line(line) {
                found.push(Found {
                    line: first + offset,
                    directive,
                    trailing: trailing && offset == 0,
                    comment_end: last,
                });
            }
        }
    }
    found.sort_by_key(|f| f.line);
    (found, comment_lines)
}

/// The definition with the shortest span that contains `line`.
fn innermost(facts: &FileFacts, line: usize) -> Option<usize> {
    facts
        .defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.lines.0 <= line && line <= d.lines.1)
        .min_by_key(|(_, d)| d.lines.1 - d.lines.0)
        .map(|(i, _)| i)
}

/// Drop the literals that start on `line`; returns them.
fn remove_strings(def: &mut Def, line: usize, removed: &mut Vec<String>) {
    let mut kept = 0;
    for at in 0..def.strings.len() {
        if def.string_lines[at] as usize == line {
            removed.push(std::mem::take(&mut def.strings[at]));
            continue;
        }
        def.strings.swap(kept, at);
        def.string_lines.swap(kept, at);
        kept += 1;
    }
    def.strings.truncate(kept);
    def.string_lines.truncate(kept);
}

/// Read the directives of `tree`, apply them to `facts` and keep them in
/// `facts.annotations`. Runs after the lines are classified.
pub fn apply(facts: &mut FileFacts, tree: &Tree, source: &str) {
    if !source.contains("aster:") {
        return;
    }
    let (found, comment_lines) = scan(tree, source);
    if found.is_empty() {
        return;
    }
    let lines: Vec<&str> = source.split('\n').collect();
    for item in found {
        // The line the directive speaks about.
        let target = if item.trailing {
            item.line
        } else {
            (item.comment_end + 1..=lines.len())
                .find(|&n| !lines[n - 1].trim().is_empty() && !comment_lines.contains(&n))
                .unwrap_or(item.line)
        };
        let mut annotation = Annotation {
            line: item.line,
            directive: item.directive,
            target,
            owner: None,
            suppressed: Vec::new(),
        };
        let class = match &annotation.directive {
            Directive::NotARead => {
                let mut removed = Vec::new();
                for def in facts.defs.iter_mut().chain([&mut facts.top]) {
                    remove_strings(def, target, &mut removed);
                }
                annotation.suppressed = removed;
                facts.class_of(target)
            }
            Directive::Reads(paths) => {
                // Inside a definition, or on the line before one.
                let owner = innermost(facts, item.line).or_else(|| innermost(facts, target));
                annotation.owner = owner.map(|o| o as u32);
                let def = match owner {
                    Some(o) => &mut facts.defs[o],
                    None => &mut facts.top,
                };
                for path in paths {
                    if !def.reads.contains(path) {
                        def.reads.push(path.clone());
                    }
                }
                owner.map_or(LineClass::Top, |o| LineClass::Def(o as u32))
            }
            Directive::Unknown(_) => LineClass::Inert,
        };
        // A comment of its own is part of the definition it speaks for.
        if class != LineClass::Inert {
            if let Some(slot) = facts.line_class.get_mut(item.line - 1) {
                if *slot == LineClass::Inert {
                    *slot = class;
                }
            }
        }
        facts.annotations.push(annotation);
    }
}

/// Whether a changed workspace path is covered by a `reads` pattern: it
/// matches the pattern, or lies beneath a directory that does.
pub fn covers(pattern: &str, changed: &std::path::Path) -> bool {
    let Ok(glob) = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
    else {
        return false;
    };
    let matcher = glob.compile_matcher();
    changed
        .ancestors()
        .any(|prefix| !prefix.as_os_str().is_empty() && matcher.is_match(prefix))
}

/// The directory a `reads` pattern starts from: the path before its first
/// glob component. A literal path is its own prefix.
pub fn literal_prefix(pattern: &str) -> std::path::PathBuf {
    pattern
        .split('/')
        .take_while(|part| !part.contains(['*', '?', '[', '{']))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_directives_from_each_comment_style() {
        let parse = |line: &str| parse_line(line);
        assert_eq!(parse("# aster: not-a-read"), Some(Directive::NotARead));
        assert_eq!(parse("  // aster: not-a-read"), Some(Directive::NotARead));
        assert_eq!(parse("/* aster: not-a-read */"), Some(Directive::NotARead));
        assert_eq!(parse(" * aster: not-a-read"), Some(Directive::NotARead));
        assert_eq!(
            parse("/// aster: not-a-read -- a fixture"),
            Some(Directive::NotARead)
        );
        assert_eq!(
            parse("# aster: reads ./a/b //c/d/ e/*.yaml -- why"),
            Some(Directive::Reads(vec![
                "a/b".into(),
                "c/d".into(),
                "e/*.yaml".into()
            ]))
        );
        assert_eq!(
            parse("# aster: nope"),
            Some(Directive::Unknown("nope".into()))
        );
        // The directive starts the comment; prose that mentions it does not count.
        assert_eq!(parse("# see aster: not-a-read"), None);
        assert_eq!(parse("# plain comment"), None);
    }

    #[test]
    fn patterns_cover_files_and_directories() {
        use std::path::Path;
        assert!(covers("config", Path::new("config/app.yaml")));
        assert!(covers("config/*.yaml", Path::new("config/app.yaml")));
        assert!(!covers("config/*.yaml", Path::new("config/sub/app.yaml")));
        assert!(covers("fixtures/**", Path::new("fixtures/a/b.json")));
        assert!(!covers("config", Path::new("configs/app.yaml")));
        assert_eq!(
            literal_prefix("a/b/*.yaml"),
            std::path::PathBuf::from("a/b")
        );
        assert_eq!(
            literal_prefix("a/b.txt"),
            std::path::PathBuf::from("a/b.txt")
        );
    }

    use super::super::{elixir, go, js, python, rust};
    use std::path::Path;

    type Extract = fn(&Path, &str) -> FileFacts;

    /// A test per language that writes `config/app.yaml` (annotated) and
    /// reads `config/other.yaml`.
    fn samples() -> Vec<(&'static str, Extract, &'static str, usize)> {
        vec![
            (
                "t_test.exs",
                elixir::extract,
                "defmodule T do\n  use ExUnit.Case\n  test \"w\" do\n    # aster: not-a-read\n    File.write!(Path.join(dir, \"config/app.yaml\"), \"x\")\n    File.read!(\"config/other.yaml\")\n  end\nend\n",
                4,
            ),
            (
                "t.test.ts",
                js::extract,
                "it(\"w\", () => {\n  // aster: not-a-read\n  writeFileSync(join(dir, \"config/app.yaml\"), x);\n  readFileSync(\"config/other.yaml\");\n});\n",
                2,
            ),
            (
                "test_t.py",
                python::extract,
                "def test_w(tmp):\n    # aster: not-a-read\n    os.makedirs(os.path.join(tmp, \"config/app.yaml\"))\n    open(\"config/other.yaml\")\n",
                2,
            ),
            (
                "t_test.go",
                go::extract,
                "package t\n\nfunc TestW(t *testing.T) {\n\t// aster: not-a-read\n\tos.WriteFile(filepath.Join(dir, \"config/app.yaml\"), nil, 0o644)\n\tos.ReadFile(\"config/other.yaml\")\n}\n",
                4,
            ),
            (
                "t.rs",
                rust::extract,
                "#[test]\nfn w() {\n    // aster: not-a-read\n    fs::write(dir.join(\"config/app.yaml\"), x).unwrap();\n    fs::read(\"config/other.yaml\").unwrap();\n}\n",
                3,
            ),
        ]
    }

    fn all_strings(facts: &FileFacts) -> Vec<&str> {
        facts
            .defs
            .iter()
            .chain([&facts.top])
            .flat_map(|d| d.strings.iter().map(String::as_str))
            .collect()
    }

    #[test]
    fn not_a_read_drops_the_literals_of_the_next_code_line_in_every_language() {
        for (name, extract, source, comment) in samples() {
            let facts = extract(Path::new(name), source);
            let strings = all_strings(&facts);
            assert!(
                strings.contains(&"config/other.yaml"),
                "{name}: {strings:?}"
            );
            assert!(!strings.contains(&"config/app.yaml"), "{name}: {strings:?}");
            let [annotation] = facts.annotations.as_slice() else {
                panic!("{name}: {:?}", facts.annotations);
            };
            assert_eq!(annotation.line, comment, "{name}");
            assert_eq!(annotation.target, comment + 1, "{name}");
            assert_eq!(annotation.suppressed, ["config/app.yaml"], "{name}");
            // The comment belongs to the definition, so editing it is a
            // change to that definition and not an inert one.
            assert!(
                matches!(facts.class_of(comment), LineClass::Def(_)),
                "{name}: {:?}",
                facts.class_of(comment)
            );
        }
    }

    #[test]
    fn without_the_comment_the_literal_is_recorded() {
        for (name, extract, source, comment) in samples() {
            let plain: String = source
                .lines()
                .enumerate()
                .filter(|(i, _)| i + 1 != comment)
                .map(|(_, line)| format!("{line}\n"))
                .collect();
            let facts = extract(Path::new(name), &plain);
            assert!(all_strings(&facts).contains(&"config/app.yaml"), "{name}");
            assert!(facts.annotations.is_empty(), "{name}");
        }
    }

    #[test]
    fn a_trailing_comment_covers_its_own_line() {
        let source = "it(\"w\", () => {\n  writeFileSync(join(dir, \"config/app.yaml\"), x); // aster: not-a-read\n  readFileSync(\"config/other.yaml\");\n});\n";
        let facts = js::extract(Path::new("t.test.ts"), source);
        let strings = all_strings(&facts);
        assert_eq!(facts.annotations[0].target, 2);
        assert!(!strings.contains(&"config/app.yaml"));
        assert!(strings.contains(&"config/other.yaml"));
        // A block comment works the same way.
        let block = source.replace("// aster: not-a-read", "/* aster: not-a-read */");
        let facts = js::extract(Path::new("t.test.ts"), &block);
        assert!(!all_strings(&facts).contains(&"config/app.yaml"));
    }

    #[test]
    fn reads_attaches_paths_to_the_enclosing_or_following_definition() {
        let inside = js::extract(
            Path::new("t.test.ts"),
            "it(\"a\", () => {\n  // aster: reads ./schemas/**/*.json fixtures/\n});\nit(\"b\", () => {});\n",
        );
        let reads: Vec<_> = inside
            .defs
            .iter()
            .map(|d| (d.name.as_str(), &d.reads))
            .collect();
        assert_eq!(reads[0].1, &["schemas/**/*.json", "fixtures"], "{reads:?}");
        assert!(reads[1].1.is_empty());
        assert!(matches!(inside.class_of(2), LineClass::Def(0)));

        let before = python::extract(
            Path::new("test_t.py"),
            "# aster: reads data/a.csv\ndef test_a():\n    pass\n\ndef test_b():\n    pass\n",
        );
        assert_eq!(before.defs[0].reads, ["data/a.csv"]);
        assert!(before.defs[1].reads.is_empty());

        let elixir = elixir::extract(
            Path::new("t_test.exs"),
            "defmodule T do\n  use ExUnit.Case\n  test \"a\" do\n    # aster: reads config/dev.exs config/prod.exs\n    :ok\n  end\nend\n",
        );
        let test = elixir.defs.iter().find(|d| d.is_test()).unwrap();
        assert_eq!(test.reads, ["config/dev.exs", "config/prod.exs"]);

        let go = go::extract(
            Path::new("t_test.go"),
            "package t\n\n// aster: reads testdata/in.json\nfunc TestA(t *testing.T) {}\n",
        );
        assert_eq!(go.defs[0].reads, ["testdata/in.json"]);

        let rust = rust::extract(
            Path::new("t.rs"),
            "#[test]\nfn a() {\n    // aster: reads tests/data\n}\n",
        );
        assert_eq!(rust.defs[0].reads, ["tests/data"]);
    }

    #[test]
    fn unknown_and_empty_directives_are_kept_for_the_linter() {
        let facts = python::extract(
            Path::new("test_t.py"),
            "def test_a():\n    # aster: nope\n    # aster: reads\n    pass\n",
        );
        assert_eq!(
            facts.annotations[0].directive,
            Directive::Unknown("nope".into())
        );
        assert_eq!(facts.annotations[1].directive, Directive::Reads(vec![]));
        // An unknown directive stays an inert comment.
        assert_eq!(facts.class_of(2), LineClass::Inert);
    }
}
