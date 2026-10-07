//! Per-file facts extracted from a syntax tree: the definitions a file
//! contains, the names each one refers to, and the files each one draws
//! those names from.

use std::collections::BTreeSet;
use std::path::Path;

/// Languages that resolve names against each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    Elixir,
    Js,
    Go,
    Python,
}

impl Family {
    /// The family a source file belongs to, by extension.
    pub fn of(path: &Path) -> Option<Family> {
        match path.extension()?.to_str()? {
            "ex" | "exs" => Some(Family::Elixir),
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Family::Js),
            "go" => Some(Family::Go),
            "py" => Some(Family::Python),
            _ => None,
        }
    }

    /// The family a project's plugin analyses, if any.
    pub fn of_plugin(plugin: &str) -> Option<Family> {
        match plugin {
            "elixir" => Some(Family::Elixir),
            "nodejs" => Some(Family::Js),
            "go" => Some(Family::Go),
            "python" => Some(Family::Python),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Function,
    /// Reached through a value rather than an import (a method, a protocol
    /// or behaviour implementation), so callers need not name its file.
    Method,
    Type,
    Const,
    /// Expands at the call site; a change alters every user of the file.
    Macro,
    Test,
}

/// One definition, or the code of a file outside every definition.
#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    /// Other names the same definition is known by (`default`).
    pub aliases: Vec<String>,
    pub kind: DefKind,
    /// 1-based inclusive line span.
    pub lines: (usize, usize),
    /// Every identifier the definition mentions.
    pub refs: BTreeSet<String>,
    /// Calls qualified by the module they go to, as `(module, function)`.
    /// Only languages that set [`FileFacts::qualified`] fill this.
    pub calls: BTreeSet<(String, String)>,
    /// Names that may be dispatched on at run time: atoms, and functions
    /// called on a value (`handler.run()`).
    pub atoms: BTreeSet<String>,
    /// Unresolved specifiers of the files it draws names from: import
    /// specifiers, module names, package paths.
    pub uses: BTreeSet<String>,
    /// String literals that could name a file.
    pub strings: Vec<String>,
    /// Invoked by a framework or runtime rather than by name.
    pub callback: bool,
    /// Specifiers of files whose users reach this definition without naming
    /// its own file: the protocol and type of an Elixir `defimpl`.
    pub via: Vec<String>,
    /// The URL path pattern this definition serves, when it is a route
    /// declaration (`/orgs/:org/members`). Requests reach it by path, not
    /// by name.
    pub route: Option<String>,
    /// URL-like string literals (`/orgs/\0/members`, with `\0` standing
    /// for an interpolated part): the requests this definition may send.
    pub paths: Vec<String>,
}

impl Def {
    pub fn new(name: impl Into<String>, kind: DefKind, lines: (usize, usize)) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            kind,
            lines,
            refs: BTreeSet::new(),
            calls: BTreeSet::new(),
            atoms: BTreeSet::new(),
            uses: BTreeSet::new(),
            strings: Vec::new(),
            callback: false,
            via: Vec::new(),
            route: None,
            paths: Vec::new(),
        }
    }

    pub fn is_test(&self) -> bool {
        self.kind == DefKind::Test
    }

    /// Record a string literal when it could name a file.
    pub fn string(&mut self, text: &str) {
        if text.len() <= 4000 && (text.contains('.') || text.contains('/')) {
            self.strings.push(text.to_string());
        }
    }
}

/// How one source line maps onto the file's definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineClass {
    /// Blank, comment or documentation.
    Inert,
    /// Code outside every definition.
    Top,
    /// Inside the definition with this index.
    Def(u32),
    /// On the import with this index.
    Import(u32),
}

/// A statement that brings names into the file. Changing one matters to the
/// definitions that use those names, not to the whole file.
#[derive(Debug, Clone, Default)]
pub struct ImportLine {
    /// 1-based inclusive line span.
    pub lines: (usize, usize),
    /// Local names it binds.
    pub names: Vec<String>,
    /// Specifiers it brings in.
    pub specs: Vec<String>,
    /// Punctuation around a group of imports; a change to it alone means
    /// nothing.
    pub structural: bool,
}

#[derive(Debug, Clone)]
pub struct FileFacts {
    pub family: Family,
    pub defs: Vec<Def>,
    /// Code outside every definition, as a nameless definition.
    pub top: Def,
    /// Specifiers this file re-exports: importers of this file use them too.
    pub reexports: Vec<String>,
    /// Specifiers every definition in the file uses (wildcard imports).
    pub uses_all: Vec<String>,
    /// Elixir modules defined here, or the Go package name.
    pub modules: Vec<String>,
    /// Import statements outside every definition.
    pub imports: Vec<ImportLine>,
    /// Go imports as `(alias, path)`; the qualifier of an unaliased import
    /// is only known once the imported package has been read.
    pub go_imports: Vec<(Option<String>, String)>,
    pub line_class: Vec<LineClass>,
    pub is_test_file: bool,
    /// Calls into other files are qualified by module (`Module.function`),
    /// so a bare name refers to this file or to something it imports, and
    /// two modules' functions of the same name are told apart.
    pub qualified: bool,
    /// Loads code by a computed name, which no static analysis can follow.
    pub dynamic: Option<&'static str>,
    /// `//go:embed` patterns, relative to the file's directory, with the
    /// definition each one fills.
    pub embeds: Vec<(String, Owner)>,
    pub parse_error: bool,
}

impl FileFacts {
    pub fn new(family: Family) -> Self {
        Self {
            family,
            defs: Vec::new(),
            top: Def::new("", DefKind::Const, (0, 0)),
            reexports: Vec::new(),
            uses_all: Vec::new(),
            modules: Vec::new(),
            imports: Vec::new(),
            go_imports: Vec::new(),
            line_class: Vec::new(),
            is_test_file: false,
            qualified: false,
            dynamic: None,
            embeds: Vec::new(),
            parse_error: false,
        }
    }

    /// Classify each line once every definition is known. `inert` holds the
    /// byte ranges of comments and documentation.
    pub fn classify_lines(&mut self, source: &str, inert: &[(usize, usize)]) {
        let mut mask = vec![false; source.len()];
        for &(start, end) in inert {
            for flag in &mut mask[start.min(source.len())..end.min(source.len())] {
                *flag = true;
            }
        }
        let mut classes = Vec::new();
        let mut offset = 0;
        for (index, line) in source.split('\n').enumerate() {
            let number = index + 1;
            let inert_line = line
                .bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_whitespace() || mask[offset + i]);
            offset += line.len() + 1;
            if inert_line {
                classes.push(LineClass::Inert);
                continue;
            }
            // The innermost definition containing the line.
            let owner = self
                .defs
                .iter()
                .enumerate()
                .filter(|(_, d)| d.lines.0 <= number && number <= d.lines.1)
                .min_by_key(|(_, d)| d.lines.1 - d.lines.0);
            let import = self
                .imports
                .iter()
                .enumerate()
                .filter(|(_, i)| i.lines.0 <= number && number <= i.lines.1)
                .min_by_key(|(_, i)| i.lines.1 - i.lines.0);
            classes.push(match (owner, import) {
                (Some((i, _)), _) => LineClass::Def(i as u32),
                (None, Some((i, _))) => LineClass::Import(i as u32),
                (None, None) => LineClass::Top,
            });
        }
        self.line_class = classes;
    }

    /// The class of 1-based line `line`; lines past the end are inert.
    pub fn class_of(&self, line: usize) -> LineClass {
        self.line_class
            .get(line.wrapping_sub(1))
            .copied()
            .unwrap_or(LineClass::Inert)
    }
}

/// Stands for an interpolated part of a path literal.
pub const WILDCARD: char = '\0';

/// Whether a request path literal could be served by a route pattern.
///
/// Segments must agree one by one, where a pattern parameter (`:id`, `*rest`)
/// or an interpolated literal part matches anything. A literal that is
/// shorter than the pattern matches as a prefix, because tests often build a
/// path from a base (`base <> "/members"`); that needs two fixed segments,
/// so that `/` or `/api` does not match every route.
pub fn path_reaches(literal: &str, pattern: &str) -> bool {
    let segments = |path: &str| -> Vec<String> {
        path.split(['?', '#'])
            .next()
            .unwrap_or("")
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    let literal = segments(literal);
    let pattern = segments(pattern);
    let open = |segment: &str| segment.contains(WILDCARD);
    let glob = pattern.last().is_some_and(|s| s.starts_with('*'));
    if literal.len() > pattern.len() && !glob {
        // An interpolated tail may hold further segments.
        return false;
    }
    let agree = literal.iter().zip(&pattern).all(|(have, want)| {
        want.starts_with(':') || want.starts_with('*') || open(have) || have == want
    });
    if !agree {
        return false;
    }
    if literal.len() >= pattern.len() {
        return true;
    }
    literal.iter().filter(|s| !open(s)).count() >= 2
}

/// Where the walk currently records references: a definition or the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Owner {
    #[default]
    Top,
    Def(usize),
}

impl FileFacts {
    pub fn owner(&mut self, owner: Owner) -> &mut Def {
        match owner {
            Owner::Top => &mut self.top,
            Owner::Def(i) => &mut self.defs[i],
        }
    }

    pub fn push(&mut self, def: Def) -> Owner {
        self.defs.push(def);
        Owner::Def(self.defs.len() - 1)
    }
}

pub fn text<'a>(node: tree_sitter::Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub fn span(node: tree_sitter::Node) -> (usize, usize) {
    let end = node.end_position();
    // A node ending at column 0 stops on the previous line.
    let last = if end.column == 0 && end.row > node.start_position().row {
        end.row
    } else {
        end.row + 1
    };
    (node.start_position().row + 1, last)
}

pub fn children(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// Parse `source` with `language`; `None` if the parser gives up.
pub fn parse(language: tree_sitter::Language, source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(source, None)
}
