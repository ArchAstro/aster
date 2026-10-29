//! What a string literal is an argument of, for `aster check`.
//!
//! The facts record a literal without its surroundings. This module parses a
//! file again and reports, for the literals asked about, whether each is an
//! argument of a call that writes (`File.write!`, `os.makedirs`, `fs::write`)
//! or a later argument of a path join (`Path.join(base, "x")`,
//! `base / "x"`). The names are the common standard-library ones of each
//! language; a write through a function of the project's own is not seen.

use super::facts::{text, Family};
use std::collections::HashSet;
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// An argument of a call that writes to the path.
    Write { callee: String },
    /// A later argument of a path join; `base` is the text of the first.
    Join { callee: String, base: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub line: usize,
    pub text: String,
    pub role: Role,
}

fn language(family: Family, path: &Path) -> tree_sitter::Language {
    match family {
        Family::Elixir => tree_sitter_elixir::LANGUAGE.into(),
        Family::Go => tree_sitter_go::LANGUAGE.into(),
        Family::Python => tree_sitter_python::LANGUAGE.into(),
        Family::Rust => tree_sitter_rust::LANGUAGE.into(),
        Family::Js => {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(extension, "ts" | "mts" | "cts") {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            }
        }
    }
}

/// The kind of the node that holds a literal's text.
fn content_kind(family: Family) -> &'static [&'static str] {
    match family {
        Family::Elixir => &["quoted_content"],
        Family::Js => &["string_fragment"],
        Family::Python | Family::Rust => &["string_content"],
        Family::Go => &[
            "interpreted_string_literal_content",
            "raw_string_literal_content",
        ],
    }
}

/// The roles of the literals in `wanted` (line and text), in `source`.
pub fn sites(
    family: Family,
    path: &Path,
    source: &str,
    wanted: &HashSet<(usize, String)>,
) -> Vec<Site> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language(family, path)).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let kinds = content_kind(family);
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if kinds.contains(&node.kind()) {
            let line = node.start_position().row + 1;
            let literal = text(node, source);
            if wanted.contains(&(line, literal.to_string())) {
                if let Some(role) = role(family, node, source) {
                    found.push(Site {
                        line,
                        text: literal.to_string(),
                        role,
                    });
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    found.sort_by(|a, b| (a.line, &a.text).cmp(&(b.line, &b.text)));
    found
}

fn is_list(family: Family, kind: &str) -> bool {
    match family {
        Family::Elixir | Family::Python => kind == "list",
        Family::Js => kind == "array",
        Family::Rust => kind == "array_expression",
        Family::Go => false,
    }
}

fn is_call(family: Family, kind: &str) -> bool {
    match family {
        Family::Elixir | Family::Python => kind == "call",
        Family::Js | Family::Go | Family::Rust => kind == "call_expression",
    }
}

/// The callee of a call node, and the receiver of a method call.
fn callee<'a>(family: Family, call: Node<'a>) -> Option<Node<'a>> {
    match family {
        Family::Elixir => call.child_by_field_name("target"),
        _ => call.child_by_field_name("function"),
    }
}

fn arguments<'a>(call: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = call.walk();
    let Some(list) = call
        .children(&mut cursor)
        .find(|c| matches!(c.kind(), "arguments" | "argument_list"))
    else {
        return Vec::new();
    };
    let mut cursor = list.walk();
    list.named_children(&mut cursor)
        .filter(|c| !c.kind().contains("comment"))
        .collect()
}

fn contains(outer: Node, inner: Node) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}

/// The role of the literal whose content node is `content`.
fn role(family: Family, content: Node, source: &str) -> Option<Role> {
    let mut child = content;
    // Where the literal sits in a list that is itself an argument:
    // `Path.join(["a", "b"])`.
    let mut in_list: Option<(usize, String)> = None;
    while let Some(parent) = child.parent() {
        // `base / "x"` in Python.
        if family == Family::Python && parent.kind() == "binary_operator" {
            let operator = parent
                .child_by_field_name("operator")
                .map(|o| text(o, source));
            if operator == Some("/") {
                let right = parent.child_by_field_name("right")?;
                if contains(right, content) {
                    let base = text(parent.child_by_field_name("left")?, source);
                    return Some(Role::Join {
                        callee: "/".into(),
                        base: base.to_string(),
                    });
                }
            }
        }
        if is_list(family, parent.kind()) && in_list.is_none() {
            let mut cursor = parent.walk();
            let elements: Vec<Node> = parent.named_children(&mut cursor).collect();
            in_list = elements
                .iter()
                .position(|c| contains(*c, content))
                .map(|at| (at, text(elements[0], source).to_string()));
        }
        if is_call(family, parent.kind()) {
            let args = arguments(parent);
            if let Some(at) = args.iter().position(|a| contains(*a, content)) {
                return call_role(family, parent, &args, at, in_list.as_ref(), source);
            }
        }
        child = parent;
    }
    None
}

/// `File.write!` splits into the qualifier `File` and the name `write!`.
fn split_callee(callee: &str) -> (Option<&str>, &str) {
    let mut parts = callee.rsplit(['.', ':']).filter(|p| !p.is_empty());
    let name = parts.next().unwrap_or("");
    (parts.next(), name)
}

fn call_role(
    family: Family,
    call: Node,
    args: &[Node],
    at: usize,
    in_list: Option<&(usize, String)>,
    source: &str,
) -> Option<Role> {
    let function = callee(family, call)?;
    let callee_text: String = text(function, source)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let (qualifier, name) = split_callee(&callee_text);
    // The first argument a piped call receives is not written in it.
    let piped = family == Family::Elixir
        && call.parent().is_some_and(|p| {
            p.kind() == "binary_operator"
                && p.child_by_field_name("operator").map(|o| text(o, source)) == Some("|>")
                && p.child_by_field_name("right")
                    .is_some_and(|r| r.id() == call.id())
        });
    // A list passed alone is spread over the call's arguments.
    let list_alone = args.len() == 1 && in_list.is_some();
    let index = if list_alone { in_list?.0 } else { at } + usize::from(piped);
    let piped_from = if piped {
        call.parent()
            .and_then(|p| p.child_by_field_name("left"))
            .map(|l| text(l, source).to_string())
    } else {
        None
    };
    let first_text = |args: &[Node]| match (&piped_from, in_list) {
        (Some(left), _) => Some(left.clone()),
        (None, Some((_, first))) if list_alone => Some(first.clone()),
        _ => args.first().map(|a| text(*a, source).to_string()),
    };

    // Method calls: the receiver is the first operand of a join.
    let receiver = match family {
        Family::Rust | Family::Js | Family::Go => function
            .child_by_field_name("value")
            .or_else(|| function.child_by_field_name("object"))
            .or_else(|| function.child_by_field_name("operand")),
        Family::Python => function.child_by_field_name("object"),
        Family::Elixir => None,
    };

    if let Some(write) = writes(family, qualifier, name, &callee_text, args, source) {
        if write.contains(&index) {
            return Some(Role::Write {
                callee: callee_text,
            });
        }
    }
    // `Path("x").write_text(...)`: the literal builds the path being written.
    if family == Family::Python && matches!(name, "Path" | "PurePath" | "PosixPath") {
        let method = call
            .parent()
            .filter(|p| p.kind() == "attribute")
            .and_then(|attribute| {
                let method = text(attribute.child_by_field_name("attribute")?, source);
                let outer = attribute.parent().filter(|c| c.kind() == "call")?;
                Some((method, outer))
            });
        if let Some((method, _)) = method {
            if matches!(
                method,
                "write_text"
                    | "write_bytes"
                    | "mkdir"
                    | "touch"
                    | "unlink"
                    | "rmdir"
                    | "rename"
                    | "replace"
                    | "symlink_to"
                    | "hardlink_to"
            ) {
                return Some(Role::Write {
                    callee: format!("{callee_text}(..).{method}"),
                });
            }
        }
    }
    // Joins.
    match join_base(family, qualifier, name, receiver, args, source) {
        Join::Receiver(base) => Some(Role::Join {
            callee: callee_text,
            base,
        }),
        Join::FirstArgument => (index >= 1).then(|| Role::Join {
            callee: callee_text,
            base: first_text(args).unwrap_or_default(),
        }),
        Join::No => None,
    }
}

enum Join {
    /// The base is the receiver; every argument is a later operand.
    Receiver(String),
    /// The base is the first argument.
    FirstArgument,
    No,
}

fn join_base(
    family: Family,
    qualifier: Option<&str>,
    name: &str,
    receiver: Option<Node>,
    _args: &[Node],
    source: &str,
) -> Join {
    let receiver_text = || {
        receiver
            .map(|r| text(r, source).to_string())
            .unwrap_or_default()
    };
    match family {
        Family::Elixir => {
            if qualifier == Some("Path") && name == "join" {
                Join::FirstArgument
            } else {
                Join::No
            }
        }
        Family::Js => match (qualifier, name) {
            (Some("path" | "posix" | "win32" | "Path"), "join" | "resolve") => Join::FirstArgument,
            (None, "join" | "resolve") => Join::FirstArgument,
            _ => Join::No,
        },
        Family::Python => match (qualifier, name) {
            (Some("path" | "posixpath" | "ntpath"), "join") => Join::FirstArgument,
            (None, "join") => Join::FirstArgument,
            (_, "joinpath") if receiver.is_some() => Join::Receiver(receiver_text()),
            _ => Join::No,
        },
        Family::Go => match (qualifier, name) {
            (Some("filepath" | "path"), "Join") => Join::FirstArgument,
            _ => Join::No,
        },
        Family::Rust => {
            if name == "join" && receiver.is_some() {
                Join::Receiver(receiver_text())
            } else {
                Join::No
            }
        }
    }
}

/// Argument positions of a call that name what it writes, when it is a
/// write-like call.
fn writes(
    family: Family,
    qualifier: Option<&str>,
    name: &str,
    callee: &str,
    args: &[Node],
    source: &str,
) -> Option<&'static [usize]> {
    const FIRST: &[usize] = &[0];
    const SECOND: &[usize] = &[1];
    let hit = |names: &[&str], at: &'static [usize]| names.contains(&name).then_some(at);
    match family {
        Family::Elixir if qualifier == Some("File") => hit(
            &[
                "write", "write!", "mkdir", "mkdir!", "mkdir_p", "mkdir_p!", "touch", "touch!",
                "rm", "rm!", "rm_rf", "rm_rf!", "rmdir", "rmdir!", "chmod", "chmod!",
            ],
            FIRST,
        )
        .or_else(|| {
            hit(
                &[
                    "cp", "cp!", "cp_r", "cp_r!", "rename", "rename!", "ln_s", "ln", "copy",
                    "copy!",
                ],
                SECOND,
            )
        }),
        Family::Js => hit(
            &[
                "writeFile",
                "writeFileSync",
                "appendFile",
                "appendFileSync",
                "mkdir",
                "mkdirSync",
                "mkdtemp",
                "mkdtempSync",
                "rm",
                "rmSync",
                "rmdir",
                "rmdirSync",
                "unlink",
                "unlinkSync",
                "truncate",
                "truncateSync",
                "createWriteStream",
                "outputFile",
                "outputFileSync",
                "outputJson",
                "outputJsonSync",
                "writeJson",
                "writeJsonSync",
                "ensureDir",
                "ensureDirSync",
                "ensureFile",
                "ensureFileSync",
                "mkdirp",
                "mkdirpSync",
                "emptyDir",
                "remove",
                "removeSync",
            ],
            FIRST,
        )
        .or_else(|| {
            hit(
                &[
                    "copyFile",
                    "copyFileSync",
                    "cp",
                    "cpSync",
                    "rename",
                    "renameSync",
                    "symlink",
                    "symlinkSync",
                    "link",
                    "linkSync",
                    "copy",
                    "copySync",
                    "move",
                    "moveSync",
                ],
                SECOND,
            )
        }),
        Family::Python => match qualifier {
            Some("os") => hit(
                &[
                    "mkdir",
                    "makedirs",
                    "remove",
                    "unlink",
                    "rmdir",
                    "removedirs",
                    "truncate",
                    "mknod",
                    "mkfifo",
                ],
                FIRST,
            )
            .or_else(|| hit(&["rename", "replace", "symlink", "link"], SECOND)),
            Some("shutil") => hit(&["rmtree", "make_archive"], FIRST).or_else(|| {
                hit(
                    &["copy", "copy2", "copyfile", "copytree", "copymode", "move"],
                    SECOND,
                )
            }),
            None if callee == "open" => {
                // Only an open for writing.
                let mode = args.get(1).map(|a| text(*a, source)).unwrap_or("");
                let mode = mode.trim_start_matches("mode=");
                let literal = mode.trim_matches(|c| c == '"' || c == '\'');
                (mode.len() != literal.len() && literal.contains(['w', 'a', 'x', '+']))
                    .then_some(FIRST)
            }
            _ => None,
        },
        Family::Go => match qualifier {
            Some("os") => hit(
                &[
                    "WriteFile",
                    "Mkdir",
                    "MkdirAll",
                    "Create",
                    "Remove",
                    "RemoveAll",
                    "Truncate",
                    "Chmod",
                    "Chtimes",
                    "OpenFile",
                ],
                FIRST,
            )
            .or_else(|| hit(&["Rename", "Symlink", "Link"], SECOND)),
            Some("ioutil") => hit(&["WriteFile"], FIRST),
            _ => None,
        },
        Family::Rust => match qualifier {
            Some("fs") => hit(
                &[
                    "write",
                    "create_dir",
                    "create_dir_all",
                    "remove_file",
                    "remove_dir",
                    "remove_dir_all",
                    "set_permissions",
                ],
                FIRST,
            )
            .or_else(|| {
                hit(
                    &["copy", "rename", "hard_link", "soft_link", "symlink"],
                    SECOND,
                )
            }),
            Some("File") => hit(&["create", "create_new"], FIRST),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(family: Family, name: &str, source: &str, literals: &[(usize, &str)]) -> Vec<Role> {
        let wanted = literals
            .iter()
            .map(|(line, text)| (*line, text.to_string()))
            .collect();
        sites(family, Path::new(name), source, &wanted)
            .into_iter()
            .map(|s| s.role)
            .collect()
    }

    fn write(callee: &str) -> Role {
        Role::Write {
            callee: callee.into(),
        }
    }

    fn join(callee: &str, base: &str) -> Role {
        Role::Join {
            callee: callee.into(),
            base: base.into(),
        }
    }

    #[test]
    fn elixir_writes_and_joins() {
        let source = "defmodule T do\n  def f(dir, source) do\n    File.write!(\"a/b.txt\", \"x\")\n    File.cp(\"c/d.txt\", \"e/f.txt\")\n    Path.join(source, \".devbox-setup/setup.sh\")\n    Path.join([dir, \"g/h.txt\"])\n    Path.join(\"i/j.txt\", dir)\n    \"k/l.txt\" |> File.write(\"data\")\n    File.read!(\"m/n.txt\")\n  end\nend\n";
        let at = |line: usize, text: &str| roles(Family::Elixir, "t.ex", source, &[(line, text)]);
        assert_eq!(at(3, "a/b.txt"), [write("File.write!")]);
        // Only the destination of a copy writes.
        assert_eq!(at(4, "c/d.txt"), Vec::<Role>::new());
        assert_eq!(at(4, "e/f.txt"), [write("File.cp")]);
        assert_eq!(
            at(5, ".devbox-setup/setup.sh"),
            [join("Path.join", "source")]
        );
        assert_eq!(at(6, "g/h.txt"), [join("Path.join", "dir")]);
        // The first operand is the base, not a later one.
        assert_eq!(at(7, "i/j.txt"), Vec::<Role>::new());
        assert_eq!(at(8, "k/l.txt"), Vec::<Role>::new());
        let piped = "defmodule T do\n  def f do\n    @root\n    |> Path.join(\"x/y.json\")\n    |> File.read!()\n  end\nend\n";
        assert_eq!(
            roles(Family::Elixir, "t.ex", piped, &[(4, "x/y.json")]),
            [join("Path.join", "@root")]
        );
        assert_eq!(at(9, "m/n.txt"), Vec::<Role>::new());
    }

    #[test]
    fn javascript_writes_and_joins() {
        let source = "it('x', async () => {\n  await writeFile(join(dir, 'a/b.txt'), x);\n  fs.copyFileSync('c/d.txt', 'e/f.txt');\n  path.resolve(root, 'g/h.txt');\n  readFileSync('i/j.txt');\n  fs.mkdirSync(`k/l`);\n});\n";
        let at = |line: usize, text: &str| roles(Family::Js, "t.test.ts", source, &[(line, text)]);
        assert_eq!(at(2, "a/b.txt"), [join("join", "dir")]);
        assert_eq!(at(3, "c/d.txt"), Vec::<Role>::new());
        assert_eq!(at(3, "e/f.txt"), [write("fs.copyFileSync")]);
        assert_eq!(at(4, "g/h.txt"), [join("path.resolve", "root")]);
        assert_eq!(at(5, "i/j.txt"), Vec::<Role>::new());
    }

    #[test]
    fn python_writes_and_joins() {
        let source = "def test_x(tmp):\n    os.makedirs('a/b')\n    shutil.copy('c/d.txt', 'e/f.txt')\n    os.path.join(tmp, 'g/h.txt')\n    (tmp / 'i/j.txt').write_text('x')\n    Path('k/l.txt').write_text('x')\n    open('m/n.txt', 'w')\n    open('o/p.txt')\n    tmp.joinpath('q/r.txt')\n";
        let at =
            |line: usize, text: &str| roles(Family::Python, "test_t.py", source, &[(line, text)]);
        assert_eq!(at(2, "a/b"), [write("os.makedirs")]);
        assert_eq!(at(3, "e/f.txt"), [write("shutil.copy")]);
        assert_eq!(at(3, "c/d.txt"), Vec::<Role>::new());
        assert_eq!(at(4, "g/h.txt"), [join("os.path.join", "tmp")]);
        assert_eq!(at(5, "i/j.txt"), [join("/", "tmp")]);
        assert_eq!(at(6, "k/l.txt"), [write("Path(..).write_text")]);
        assert_eq!(at(7, "m/n.txt"), [write("open")]);
        assert_eq!(at(8, "o/p.txt"), Vec::<Role>::new());
        assert_eq!(at(9, "q/r.txt"), [join("tmp.joinpath", "tmp")]);
    }

    #[test]
    fn go_writes_and_joins() {
        let source = "package t\n\nfunc TestX(t *testing.T) {\n\tos.WriteFile(\"a/b.txt\", nil, 0o644)\n\tfilepath.Join(dir, \"c/d.txt\")\n\tos.Rename(\"e/f.txt\", \"g/h.txt\")\n\tos.ReadFile(\"i/j.txt\")\n}\n";
        let at = |line: usize, text: &str| roles(Family::Go, "t_test.go", source, &[(line, text)]);
        assert_eq!(at(4, "a/b.txt"), [write("os.WriteFile")]);
        assert_eq!(at(5, "c/d.txt"), [join("filepath.Join", "dir")]);
        assert_eq!(at(6, "e/f.txt"), Vec::<Role>::new());
        assert_eq!(at(6, "g/h.txt"), [write("os.Rename")]);
        assert_eq!(at(7, "i/j.txt"), Vec::<Role>::new());
    }

    #[test]
    fn rust_writes_and_joins() {
        let source = "#[test]\nfn x() {\n    fs::write(\"a/b.txt\", x).unwrap();\n    std::fs::create_dir_all(dir.join(\"c/d\")).unwrap();\n    fs::copy(\"e/f.txt\", \"g/h.txt\").unwrap();\n    fs::read(\"i/j.txt\").unwrap();\n    File::create(\"k/l.txt\").unwrap();\n}\n";
        let at = |line: usize, text: &str| roles(Family::Rust, "t.rs", source, &[(line, text)]);
        assert_eq!(at(3, "a/b.txt"), [write("fs::write")]);
        // The join is the nearest call; its base is the receiver.
        assert_eq!(at(4, "c/d"), [join("dir.join", "dir")]);
        assert_eq!(at(5, "e/f.txt"), Vec::<Role>::new());
        assert_eq!(at(5, "g/h.txt"), [write("fs::copy")]);
        assert_eq!(at(6, "i/j.txt"), Vec::<Role>::new());
        assert_eq!(at(7, "k/l.txt"), [write("File::create")]);
    }
}
