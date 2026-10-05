//! Related-test selection for `cargo test` targets under
//! `aster affected --only-affected-files`.
//!
//! Given the files that changed inside a Cargo package, this module works out
//! which tests can observe the change and rewrites the target's `cargo test`
//! command to run only those. The analysis is textual and deterministic: it
//! never invokes rustc or cargo. When it cannot map a change it runs more, not
//! less.
//!
//! 1. Each crate target (lib, bins, integration tests, benches, examples and
//!    the build script) is walked from its root file through `mod x;`
//!    declarations (honouring `#[path]`), which assigns every source file to a
//!    target and, for the library, to a module path.
//! 2. Every library module records the modules it references: `crate::`,
//!    `$crate::`, `super::` and `self::` paths, bare paths through a child
//!    module, a `use` alias, or a glob import, and invocations of
//!    `macro_rules!` macros defined in another module.
//! 3. Changed library modules are the seeds. A changed module that implements
//!    a type from another module (`impl Trait for other::Type`) also seeds
//!    that module, since users of the type can observe the impl; a blanket
//!    impl (`impl<T: Bound> Trait for T`) seeds the trait's module. The
//!    related modules are every module that transitively references a seed.
//! 4. Library unit tests run filtered to the related module paths. `#[test]`
//!    functions in the crate root file run by exact name whenever any module
//!    is related, and doctests run unfiltered (doc comments are not
//!    analysed). Bins, integration tests, benches and examples run when one
//!    of their files changed or they reference a related module through the
//!    crate name. A test that runs a binary (`CARGO_BIN_EXE_*`, assert_cmd's
//!    `cargo_bin`, escargot) runs whenever any binary is related.
//! 5. A non-Rust file maps to the Rust sources whose string literals name it
//!    as a path, or pass it or one of its directories to a path call
//!    (`include_str!`, `dir.join("fixtures")`).
//! 6. Other workspace members that changed, or that depend on a changed
//!    package through a path dependency, run in full.
//!
//! Anything the analysis cannot map runs the original command unchanged:
//! manifests, lockfiles, toolchain files, the build script, the library root
//! and files it names, Rust files outside every target, non-Rust files no
//! source names, declared targets whose root file is missing, and a change
//! that reaches no test. With files changed, the target is never skipped.

use super::{FilesListPlan, FilesListSelection};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use toml::Value;

/// Above this many module filters the command line stops being useful.
const MAX_FILTERS: usize = 400;
/// When at least this share of library modules is related, run them all.
const FULL_RUN_SHARE: f64 = 0.6;
/// The share rule only applies to libraries with at least this many modules.
const FULL_RUN_MIN_MODULES: usize = 10;
/// Explanation lists longer than this are truncated.
const EXPLAIN_LIST_LIMIT: usize = 40;

/// Choose the tests a `cargo test` command must run for `files` (relative to
/// `package_dir`). Commands other than `cargo test` run unchanged.
pub(crate) fn select(package_dir: &Path, command: &str, files: &[PathBuf]) -> FilesListSelection {
    let cargo = match CargoTest::parse(command) {
        Ok(cargo) => cargo,
        Err(reason) => return full(vec![format!("full run: {reason}")]),
    };
    let package = match Package::load(package_dir) {
        Ok(package) => package,
        Err(error) => return full(vec![format!("full run: could not analyse crate: {error}")]),
    };
    let workspace = Workspace::load(&package);
    Selector {
        cargo: &cargo,
        package: &package,
        workspace: &workspace,
        explanation: Vec::new(),
    }
    .select(files)
}

fn full(explanation: Vec<String>) -> FilesListSelection {
    FilesListSelection {
        plan: FilesListPlan::Full,
        explanation,
    }
}

// ============================================================================
// Lexer
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    PathSep,
    Punct(char),
    Str(String),
}

/// Tokenise Rust source. Comments are dropped; string literal contents are
/// kept (escapes roughly decoded) because data-file mentions live there.
fn lex(src: &str) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < n && depth > 0 {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if let Some((content, next)) = lex_string(&chars, i) {
            toks.push(Tok::Str(content));
            i = next;
            continue;
        }
        if c == '\'' {
            i = skip_char_or_lifetime(&chars, i);
            continue;
        }
        if c == 'b' && chars.get(i + 1) == Some(&'\'') {
            i = skip_char_or_lifetime(&chars, i + 1);
            continue;
        }
        if c == '$' || c == '_' || c.is_alphabetic() {
            let raw_ident = c == 'r'
                && chars.get(i + 1) == Some(&'#')
                && chars
                    .get(i + 2)
                    .is_some_and(|next| *next == '_' || next.is_alphabetic());
            let start = if raw_ident { i + 2 } else { i };
            let mut j = start + 1;
            while j < n && (chars[j] == '_' || chars[j].is_alphanumeric()) {
                j += 1;
            }
            toks.push(Tok::Ident(chars[start..j].iter().collect()));
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            while i < n && (chars[i] == '_' || chars[i].is_alphanumeric()) {
                i += 1;
            }
            continue;
        }
        if c == ':' && chars.get(i + 1) == Some(&':') {
            toks.push(Tok::PathSep);
            i += 2;
            continue;
        }
        toks.push(Tok::Punct(c));
        i += 1;
    }
    toks
}

/// Lex a string literal (`"…"`, `b"…"`, `c"…"`, `r#"…"#`, …) starting at `i`.
fn lex_string(chars: &[char], i: usize) -> Option<(String, usize)> {
    let n = chars.len();
    let mut j = i;
    if j < n && (chars[j] == 'b' || chars[j] == 'c') {
        j += 1;
    }
    if j < n && chars[j] == 'r' {
        j += 1;
        let mut hashes = 0;
        while j < n && chars[j] == '#' {
            hashes += 1;
            j += 1;
        }
        if j >= n || chars[j] != '"' {
            return None;
        }
        j += 1;
        let start = j;
        while j < n {
            if chars[j] == '"' && (0..hashes).all(|k| chars.get(j + 1 + k) == Some(&'#')) {
                return Some((chars[start..j].iter().collect(), j + 1 + hashes));
            }
            j += 1;
        }
        return Some((chars[start..].iter().collect(), n));
    }
    if j >= n || chars[j] != '"' {
        return None;
    }
    j += 1;
    let mut out = String::new();
    while j < n && chars[j] != '"' {
        if chars[j] == '\\' && j + 1 < n {
            match chars[j + 1] {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '\n' => {
                    // Line continuation: skip the newline and leading whitespace.
                    j += 2;
                    while j < n && chars[j].is_whitespace() {
                        j += 1;
                    }
                    continue;
                }
                _ => out.push(' '),
            }
            j += 2;
            continue;
        }
        out.push(chars[j]);
        j += 1;
    }
    Some((out, (j + 1).min(n)))
}

/// Skip a char literal (`'a'`, `'\n'`) or a lifetime (`'a`) starting at `i`.
fn skip_char_or_lifetime(chars: &[char], i: usize) -> usize {
    let n = chars.len();
    if chars.get(i + 1) == Some(&'\\') {
        let mut j = i + 3;
        while j < n && chars[j] != '\'' {
            j += 1;
        }
        return (j + 1).min(n);
    }
    if chars.get(i + 2) == Some(&'\'') {
        return i + 3;
    }
    let mut j = i + 1;
    while j < n && (chars[j] == '_' || chars[j].is_alphanumeric()) {
        j += 1;
    }
    j
}

// ============================================================================
// Item extraction
// ============================================================================

/// A path as written, e.g. `super::a::B` or the leaves of `use x::{y, z::*}`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawPath {
    segs: Vec<String>,
    glob: bool,
    alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// A path, with the inline `mod x { … }` blocks enclosing it.
    Path {
        inline: Vec<String>,
        raw: RawPath,
        is_use: bool,
        impl_self: bool,
    },
    /// An out-of-line `mod name;` declaration.
    ModDecl {
        inline: Vec<String>,
        name: String,
        path_attr: Option<String>,
    },
    MacroDef(String),
    MacroUse(String),
    /// A function marked `#[test]` (or `#[tokio::test]` …).
    TestFn {
        inline: Vec<String>,
        name: String,
    },
    /// A string literal, and whether it is the first argument of a
    /// path-taking call such as `.join("x")` or `read_dir("x")`.
    Str(String, bool),
}

fn is_tok(tok: Option<&Tok>, expected: &Tok) -> bool {
    tok == Some(expected)
}

/// Parse a use tree or path starting at `i` with the segments in `prefix`.
/// Leaves are appended to `out`; returns the index after the tree.
fn parse_tree(toks: &[Tok], mut i: usize, prefix: &[String], out: &mut Vec<RawPath>) -> usize {
    let mut segs = prefix.to_vec();
    if is_tok(toks.get(i), &Tok::PathSep) {
        i += 1;
    }
    loop {
        match toks.get(i) {
            Some(Tok::Ident(id)) if id != "as" => {
                segs.push(id.clone());
                i += 1;
                if is_tok(toks.get(i), &Tok::PathSep) {
                    i += 1;
                    continue;
                }
                break;
            }
            Some(Tok::Punct('{')) => {
                i += 1;
                loop {
                    match toks.get(i) {
                        None => return i,
                        Some(Tok::Punct('}')) => return i + 1,
                        _ => {}
                    }
                    let next = parse_tree(toks, i, &segs, out);
                    i = if next == i { i + 1 } else { next };
                    if is_tok(toks.get(i), &Tok::Punct(',')) {
                        i += 1;
                    }
                }
            }
            Some(Tok::Punct('*')) => {
                out.push(RawPath {
                    segs,
                    glob: true,
                    alias: None,
                });
                return i + 1;
            }
            _ => break,
        }
    }
    let mut alias = None;
    if let (Some(Tok::Ident(kw)), Some(Tok::Ident(name))) = (toks.get(i), toks.get(i + 1)) {
        if kw == "as" {
            alias = Some(name.clone());
            i += 2;
        }
    }
    if segs.last().map(String::as_str) == Some("self") {
        segs.pop();
    }
    if !segs.is_empty() {
        out.push(RawPath {
            segs,
            glob: false,
            alias,
        });
    }
    i
}

/// Index range of the self type of an item-level `impl` at `start`, if any.
fn impl_self_range(toks: &[Tok], start: usize) -> Option<(usize, usize)> {
    let item_level = match start.checked_sub(1).map(|p| &toks[p]) {
        None => true,
        Some(Tok::Punct('}' | ';' | '{' | ']')) => true,
        Some(Tok::Ident(kw)) => matches!(kw.as_str(), "unsafe" | "default"),
        _ => false,
    };
    if !item_level {
        return None;
    }
    let mut i = start + 1;
    let mut depth = 0i32;
    let mut self_start = i;
    let mut trait_start = i;
    let mut generics_end = start + 1;
    let mut for_at: Option<usize> = None;
    let mut generics_done = false;
    // `impl<T: Bound> Trait for T` implements the trait for every type, so
    // the trait path stands in for the self type.
    let range = |self_start: usize,
                 end: usize,
                 for_at: Option<usize>,
                 trait_start: usize,
                 generics_end: usize| {
        let blanket = for_at.is_some()
            && end == self_start + 1
            && matches!(&toks[self_start], Tok::Ident(param)
                if toks[start + 1..generics_end].contains(&Tok::Ident(param.clone())));
        if blanket {
            (trait_start, end)
        } else {
            (self_start, end)
        }
    };
    while i < toks.len() {
        match &toks[i] {
            Tok::Punct('<') => depth += 1,
            Tok::Punct('>')
                if !is_tok(i.checked_sub(1).and_then(|p| toks.get(p)), &Tok::Punct('-')) =>
            {
                depth -= 1;
                if depth == 0 && !generics_done {
                    generics_done = true;
                    generics_end = i;
                    self_start = i + 1;
                    trait_start = i + 1;
                }
            }
            Tok::Ident(kw)
                if depth == 0 && kw == "for" && !is_tok(toks.get(i + 1), &Tok::Punct('<')) =>
            {
                self_start = i + 1;
                for_at = Some(i);
            }
            Tok::Ident(kw) if depth == 0 && kw == "where" => {
                return Some(range(self_start, i, for_at, trait_start, generics_end))
            }
            Tok::Punct('{' | ';') if depth == 0 => {
                return Some(range(self_start, i, for_at, trait_start, generics_end))
            }
            _ => {}
        }
        if i == start + 1 && !matches!(toks[i], Tok::Punct('<')) {
            generics_done = true;
        }
        i += 1;
    }
    None
}

/// Whether the attribute opening at `open` (`[`) is a test attribute such
/// as `#[test]` or `#[tokio::test(flavor = "multi_thread")]`.
fn is_test_attr(toks: &[Tok], open: usize) -> bool {
    let mut depth = 0usize;
    for (offset, tok) in toks[open..].iter().enumerate() {
        match tok {
            Tok::Punct('[') => depth += 1,
            Tok::Punct(']') => {
                depth -= 1;
                if depth == 0 {
                    return false;
                }
            }
            Tok::Ident(id) if id == "test" && depth == 1 => {
                let i = open + offset;
                let before_ok = matches!(toks[i - 1], Tok::Punct('[') | Tok::PathSep);
                let after_ok = matches!(toks.get(i + 1), Some(Tok::Punct(']' | '(')));
                if before_ok && after_ok {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Calls whose string argument is a path: a bare name there names a file or
/// directory.
const PATH_CALLS: &[&str] = &[
    "join",
    "push",
    "read_dir",
    "read_to_string",
    "read",
    "open",
    "exists",
    "is_dir",
    "is_file",
    "with_file_name",
    "include_str",
    "include_bytes",
    "include",
];

/// Extract the module-relevant items from a token stream.
fn extract(toks: &[Tok]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut inline: Vec<(String, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut pending_inline: Option<String> = None;
    let mut pending_path_attr: Option<String> = None;
    let mut impl_range: Option<(usize, usize)> = None;
    let mut pending_test = false;
    let mut i = 0;
    let inline_names =
        |inline: &[(String, usize)]| inline.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();

    while i < toks.len() {
        match &toks[i] {
            Tok::Str(s) => {
                let path_arg = i >= 2
                    && is_tok(toks.get(i - 1), &Tok::Punct('('))
                    && matches!(&toks[i - 2], Tok::Ident(f) if PATH_CALLS.contains(&f.as_str()));
                items.push(Item::Str(s.clone(), path_arg));
            }
            Tok::Punct('#')
                if is_tok(toks.get(i + 1), &Tok::Punct('[')) && is_test_attr(toks, i + 1) =>
            {
                pending_test = true;
            }
            Tok::Punct('#') => {
                // #[path = "..."]
                if let (
                    Some(Tok::Punct('[')),
                    Some(Tok::Ident(attr)),
                    Some(Tok::Punct('=')),
                    Some(Tok::Str(value)),
                    Some(Tok::Punct(']')),
                ) = (
                    toks.get(i + 1),
                    toks.get(i + 2),
                    toks.get(i + 3),
                    toks.get(i + 4),
                    toks.get(i + 5),
                ) {
                    if attr == "path" {
                        pending_path_attr = Some(value.clone());
                        items.push(Item::Str(value.clone(), true));
                        i += 6;
                        continue;
                    }
                }
            }
            Tok::Punct('{') => {
                depth += 1;
                if let Some(name) = pending_inline.take() {
                    inline.push((name, depth));
                }
                pending_path_attr = None;
            }
            Tok::Punct('}') => {
                if inline.last().is_some_and(|(_, d)| *d == depth) {
                    inline.pop();
                }
                depth = depth.saturating_sub(1);
                pending_path_attr = None;
            }
            Tok::Punct(';') => pending_path_attr = None,
            Tok::Ident(id) => {
                let prev = i.checked_sub(1).map(|p| &toks[p]);
                let after_sep_or_dot = matches!(prev, Some(Tok::PathSep) | Some(Tok::Punct('.')));
                if id == "mod" && !after_sep_or_dot {
                    if let Some(Tok::Ident(name)) = toks.get(i + 1) {
                        match toks.get(i + 2) {
                            Some(Tok::Punct(';')) => {
                                items.push(Item::ModDecl {
                                    inline: inline_names(&inline),
                                    name: name.clone(),
                                    path_attr: pending_path_attr.take(),
                                });
                                i += 3;
                                continue;
                            }
                            Some(Tok::Punct('{')) => {
                                pending_inline = Some(name.clone());
                                i += 2;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
                if id == "fn" && pending_test {
                    pending_test = false;
                    if let Some(Tok::Ident(name)) = toks.get(i + 1) {
                        items.push(Item::TestFn {
                            inline: inline_names(&inline),
                            name: name.clone(),
                        });
                    }
                }
                if id == "impl" && !after_sep_or_dot {
                    impl_range = impl_self_range(toks, i);
                }
                if id == "macro_rules" && is_tok(toks.get(i + 1), &Tok::Punct('!')) {
                    if let Some(Tok::Ident(name)) = toks.get(i + 2) {
                        items.push(Item::MacroDef(name.clone()));
                        i += 3;
                        continue;
                    }
                }
                let is_use = id == "use" && !after_sep_or_dot;
                let in_impl_self = impl_range.is_some_and(|(s, e)| i >= s && i < e);
                let starts_path = !after_sep_or_dot
                    && id != "use"
                    && (is_tok(toks.get(i + 1), &Tok::PathSep) || in_impl_self);
                if is_use || starts_path {
                    let tree_start = if is_use { i + 1 } else { i };
                    let mut raws = Vec::new();
                    let next = parse_tree(toks, tree_start, &[], &mut raws);
                    let impl_self = in_impl_self;
                    for raw in raws {
                        items.push(Item::Path {
                            inline: inline_names(&inline),
                            raw,
                            is_use,
                            impl_self,
                        });
                    }
                    if is_tok(toks.get(next), &Tok::Punct('!')) {
                        if let Some(Tok::Ident(name)) =
                            next.checked_sub(1).and_then(|p| toks.get(p))
                        {
                            items.push(Item::MacroUse(name.clone()));
                        }
                    }
                    i = next.max(i + 1);
                    continue;
                }
                if is_tok(toks.get(i + 1), &Tok::Punct('!'))
                    && !is_tok(toks.get(i + 2), &Tok::Punct('='))
                {
                    items.push(Item::MacroUse(id.clone()));
                }
            }
            _ => {}
        }
        i += 1;
    }
    items
}

// ============================================================================
// Package model
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum AuxKind {
    Test,
    Bench,
    Example,
}

impl AuxKind {
    fn flag(self) -> &'static str {
        match self {
            AuxKind::Test => "--test",
            AuxKind::Bench => "--bench",
            AuxKind::Example => "--example",
        }
    }
}

/// The crate target a source file belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Owner {
    Lib(Vec<String>),
    Bin(String),
    Aux(AuxKind, String),
    Build,
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Lib(module) => write!(f, "module {}", module_name(module)),
            Owner::Bin(name) => write!(f, "bin {name}"),
            Owner::Aux(kind, name) => write!(f, "{} {name}", &kind.flag()[2..]),
            Owner::Build => write!(f, "build script"),
        }
    }
}

fn module_name(module: &[String]) -> String {
    if module.is_empty() {
        "crate root".to_string()
    } else {
        module.join("::")
    }
}

/// A crate target walked from its root file.
#[derive(Debug, Clone)]
struct CrateTarget {
    owner: Owner,
    /// Source files (relative to the package) and their module paths.
    files: BTreeMap<PathBuf, Vec<String>>,
}

struct Package {
    dir: PathBuf,
    name: String,
    lib_name: String,
    manifest: Value,
    lib: Option<CrateTarget>,
    others: Vec<CrateTarget>,
    /// Parsed items for every source file of every target.
    items: HashMap<PathBuf, Vec<Item>>,
    /// Owners of every source file.
    owners: HashMap<PathBuf, BTreeSet<Owner>>,
    /// Targets whose root file could not be read.
    missing_roots: Vec<String>,
}

impl Package {
    fn load(dir: &Path) -> Result<Self, String> {
        let manifest_path = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
        let manifest: Value =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
        let package = manifest
            .get("package")
            .ok_or_else(|| "Cargo.toml has no [package]".to_string())?;
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "Cargo.toml has no package name".to_string())?
            .to_string();
        let lib_name = manifest
            .get("lib")
            .and_then(|lib| lib.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| name.replace('-', "_"));

        let mut pkg = Package {
            dir: normalize(dir),
            name,
            lib_name,
            manifest,
            lib: None,
            others: Vec::new(),
            items: HashMap::new(),
            owners: HashMap::new(),
            missing_roots: Vec::new(),
        };

        let lib_root = pkg
            .manifest
            .get("lib")
            .and_then(|lib| lib.get("path"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| {
                dir.join("src/lib.rs")
                    .is_file()
                    .then(|| PathBuf::from("src/lib.rs"))
            });
        if let Some(root) = lib_root {
            pkg.lib = Some(pkg.walk_target(Owner::Lib(Vec::new()), &root));
        }

        for (owner, root) in pkg.target_roots() {
            let target = pkg.walk_target(owner, &root);
            pkg.others.push(target);
        }
        for target in pkg.lib.iter().chain(&pkg.others) {
            if target.files.is_empty() {
                pkg.missing_roots.push(target.owner.to_string());
            }
        }
        Ok(pkg)
    }

    fn auto(&self, key: &str) -> bool {
        self.manifest
            .get("package")
            .and_then(|p| p.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// Roots of every non-library target: bins, tests, benches, examples and
    /// the build script.
    fn target_roots(&self) -> Vec<(Owner, PathBuf)> {
        let mut roots: Vec<(Owner, PathBuf)> = Vec::new();
        let mut declared: HashSet<PathBuf> = HashSet::new();

        let tables = [
            ("bin", None),
            ("test", Some(AuxKind::Test)),
            ("bench", Some(AuxKind::Bench)),
            ("example", Some(AuxKind::Example)),
        ];
        for (table, kind) in tables {
            let Some(entries) = self.manifest.get(table).and_then(Value::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(name) = entry.get("name").and_then(Value::as_str) else {
                    continue;
                };
                // Cargo's inference for a target without `path`.
                let dir = match kind {
                    None => "src/bin",
                    Some(AuxKind::Test) => "tests",
                    Some(AuxKind::Bench) => "benches",
                    Some(AuxKind::Example) => "examples",
                };
                let mut candidates = Vec::new();
                if kind.is_none() && name == self.name {
                    candidates.push(PathBuf::from("src/main.rs"));
                }
                candidates.push(PathBuf::from(format!("{dir}/{name}.rs")));
                candidates.push(PathBuf::from(format!("{dir}/{name}/main.rs")));
                let path = entry
                    .get("path")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        candidates
                            .iter()
                            .find(|c| self.dir.join(c).is_file())
                            .cloned()
                            .unwrap_or_else(|| candidates[0].clone())
                    });
                let owner = match kind {
                    None => Owner::Bin(name.to_string()),
                    Some(kind) => Owner::Aux(kind, name.to_string()),
                };
                declared.insert(normalize(&path));
                roots.push((owner, path));
            }
        }

        let declared_names: HashSet<Owner> = roots.iter().map(|(o, _)| o.clone()).collect();
        let add_auto = |owner: Owner, path: PathBuf, roots: &mut Vec<(Owner, PathBuf)>| {
            if !declared.contains(&normalize(&path)) && !declared_names.contains(&owner) {
                roots.push((owner, path));
            }
        };

        if self.auto("autobins") {
            if self.dir.join("src/main.rs").is_file() {
                add_auto(
                    Owner::Bin(self.name.clone()),
                    PathBuf::from("src/main.rs"),
                    &mut roots,
                );
            }
            for (name, path) in discover_targets(&self.dir, "src/bin") {
                add_auto(Owner::Bin(name), path, &mut roots);
            }
        }
        for (key, dir, kind) in [
            ("autotests", "tests", AuxKind::Test),
            ("autobenches", "benches", AuxKind::Bench),
            ("autoexamples", "examples", AuxKind::Example),
        ] {
            if self.auto(key) {
                for (name, path) in discover_targets(&self.dir, dir) {
                    add_auto(Owner::Aux(kind, name), path, &mut roots);
                }
            }
        }

        let build = self
            .manifest
            .get("package")
            .and_then(|p| p.get("build"))
            .and_then(|b| b.as_str().map(PathBuf::from))
            .or_else(|| {
                self.dir
                    .join("build.rs")
                    .is_file()
                    .then(|| PathBuf::from("build.rs"))
            });
        if let Some(build) = build {
            roots.push((Owner::Build, build));
        }
        roots.sort();
        roots
    }

    /// Walk a crate target from its root through `mod` declarations.
    fn walk_target(&mut self, owner: Owner, root: &Path) -> CrateTarget {
        let mut files = BTreeMap::new();
        let mut queue = VecDeque::from([(normalize(root), Vec::<String>::new(), true)]);
        while let Some((file, module, is_root)) = queue.pop_front() {
            if files.contains_key(&file) {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(self.dir.join(&file)) else {
                continue;
            };
            let items = extract(&lex(&src));
            for item in &items {
                if let Item::ModDecl {
                    inline,
                    name,
                    path_attr,
                } = item
                {
                    if let Some(child) =
                        self.child_file(&file, is_root, inline, name, path_attr.as_deref())
                    {
                        let mut child_module = module.clone();
                        child_module.extend(inline.iter().cloned());
                        child_module.push(name.clone());
                        queue.push_back((child, child_module, false));
                    }
                }
            }
            self.owners
                .entry(file.clone())
                .or_default()
                .insert(match &owner {
                    Owner::Lib(_) => Owner::Lib(module.clone()),
                    other => other.clone(),
                });
            self.items.insert(file.clone(), items);
            files.insert(file, module);
        }
        CrateTarget { owner, files }
    }

    fn child_file(
        &self,
        file: &Path,
        is_root: bool,
        inline: &[String],
        name: &str,
        path_attr: Option<&str>,
    ) -> Option<PathBuf> {
        let file_dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
        let is_mod_rs = is_root || file.file_name().is_some_and(|f| f == "mod.rs");
        let mut base = if is_mod_rs {
            file_dir.clone()
        } else {
            file_dir.join(file.file_stem()?)
        };
        for segment in inline {
            base.push(segment);
        }
        if let Some(path) = path_attr {
            let dir = if inline.is_empty() { file_dir } else { base };
            let candidate = normalize(&dir.join(path));
            return self.dir.join(&candidate).is_file().then_some(candidate);
        }
        [
            base.join(format!("{name}.rs")),
            base.join(name).join("mod.rs"),
        ]
        .into_iter()
        .map(|p| normalize(&p))
        .find(|p| self.dir.join(p).is_file())
    }
}

/// Cargo's target auto-discovery in `dir`: `dir/*.rs` and `dir/*/main.rs`.
fn discover_targets(package_dir: &Path, dir: &str) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(package_dir.join(dir)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if path.is_file() {
            if let Some(stem) = file_name.strip_suffix(".rs") {
                found.push((stem.to_string(), PathBuf::from(dir).join(&file_name)));
            }
        } else if path.join("main.rs").is_file() {
            found.push((
                file_name.clone(),
                PathBuf::from(dir).join(&file_name).join("main.rs"),
            ));
        }
    }
    found.sort();
    found
}

/// Lexically normalise a relative path (`a/./b/../c` → `a/c`).
fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.pop().is_none() {
                    out.push("..".into());
                }
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    out.iter().collect()
}

// ============================================================================
// Module graph
// ============================================================================

/// How `crate::` resolves inside the file being analysed.
enum Role {
    /// A library file.
    Lib,
    /// A bin, test, bench or example: the library is reached by its name.
    External,
}

struct Resolver<'a> {
    /// Library modules backed by a file, including the root (`[]`).
    modules: &'a BTreeSet<Vec<String>>,
    lib_name: &'a str,
}

impl Resolver<'_> {
    /// The library file module that owns `path`, by longest prefix.
    fn owner_of(&self, path: &[String]) -> Option<Vec<String>> {
        (0..=path.len())
            .rev()
            .map(|len| &path[..len])
            .find(|prefix| self.modules.contains(*prefix))
            .map(<[String]>::to_vec)
    }

    /// Resolve a written path to an absolute library path.
    fn resolve(
        &self,
        segs: &[String],
        current: &[String],
        role: &Role,
        aliases: &HashMap<String, Vec<String>>,
        globs: &[Vec<String>],
    ) -> Option<Vec<String>> {
        let first = segs.first()?.as_str();
        let rest = || segs[1..].to_vec();
        if first == self.lib_name {
            return Some(rest());
        }
        if let Some(base) = aliases.get(first) {
            let mut abs = base.clone();
            abs.extend(rest());
            return Some(abs);
        }
        if !matches!(role, Role::Lib) {
            return None;
        }
        match first {
            "crate" | "$crate" => Some(rest()),
            "self" => {
                let mut abs = current.to_vec();
                abs.extend(rest());
                Some(abs)
            }
            "super" => {
                let supers = segs.iter().take_while(|s| *s == "super").count();
                let base_len = current.len().checked_sub(supers)?;
                let mut abs = current[..base_len].to_vec();
                abs.extend(segs[supers..].iter().cloned());
                Some(abs)
            }
            _ => std::iter::once(current)
                .chain(globs.iter().map(Vec::as_slice))
                .find_map(|base| {
                    let mut abs = base.to_vec();
                    abs.extend(segs.iter().cloned());
                    let owner = self.owner_of(&abs)?;
                    (owner.len() > base.len()).then_some(abs)
                }),
        }
    }
}

/// What one source file references in the library.
#[derive(Debug, Default)]
struct FileRefs {
    modules: BTreeSet<Vec<String>>,
    impl_self: BTreeSet<Vec<String>>,
    macro_uses: BTreeSet<String>,
}

fn file_refs(
    items: &[Item],
    file_module: &[String],
    role: &Role,
    resolver: &Resolver<'_>,
) -> FileRefs {
    // Pass 1: `use` aliases and glob imports.
    let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
    let mut globs: Vec<Vec<String>> = Vec::new();
    for item in items {
        if let Item::Path {
            inline,
            raw,
            is_use: true,
            ..
        } = item
        {
            let current: Vec<String> = file_module.iter().chain(inline).cloned().collect();
            let Some(abs) = resolver.resolve(&raw.segs, &current, role, &HashMap::new(), &[])
            else {
                continue;
            };
            if raw.glob {
                globs.push(abs);
            } else if let Some(name) = raw.alias.clone().or_else(|| raw.segs.last().cloned()) {
                if name != "_" {
                    aliases.insert(name, abs);
                }
            }
        }
    }

    // Pass 2: every path.
    let mut refs = FileRefs::default();
    for module in &globs {
        if let Some(owner) = resolver.owner_of(module) {
            refs.modules.insert(owner);
        }
    }
    for item in items {
        match item {
            Item::Path {
                inline,
                raw,
                impl_self,
                ..
            } => {
                let current: Vec<String> = file_module.iter().chain(inline).cloned().collect();
                let Some(abs) = resolver.resolve(&raw.segs, &current, role, &aliases, &globs)
                else {
                    continue;
                };
                if let Some(owner) = resolver.owner_of(&abs) {
                    if *impl_self {
                        refs.impl_self.insert(owner.clone());
                    }
                    refs.modules.insert(owner);
                }
            }
            Item::MacroUse(name) => {
                refs.macro_uses.insert(name.clone());
            }
            _ => {}
        }
    }
    refs
}

/// The library's module reference graph.
struct LibGraph {
    modules: BTreeSet<Vec<String>>,
    /// module → modules it references
    edges: BTreeMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// module → modules whose types it implements
    impl_self: BTreeMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// macro name → modules defining it with `macro_rules!`
    macro_defs: HashMap<String, BTreeSet<Vec<String>>>,
}

impl LibGraph {
    fn build(package: &Package) -> Self {
        let mut graph = LibGraph {
            modules: BTreeSet::new(),
            edges: BTreeMap::new(),
            impl_self: BTreeMap::new(),
            macro_defs: HashMap::new(),
        };
        let Some(lib) = &package.lib else {
            return graph;
        };
        graph.modules = lib.files.values().cloned().collect();
        graph.modules.insert(Vec::new());
        for (file, module) in &lib.files {
            for item in package.items.get(file).into_iter().flatten() {
                if let Item::MacroDef(name) = item {
                    graph
                        .macro_defs
                        .entry(name.clone())
                        .or_default()
                        .insert(module.clone());
                }
            }
        }
        let resolver = Resolver {
            modules: &graph.modules,
            lib_name: &package.lib_name,
        };
        let mut edges = BTreeMap::new();
        let mut impl_self = BTreeMap::new();
        for (file, module) in &lib.files {
            let items = package.items.get(file).map(Vec::as_slice).unwrap_or(&[]);
            let refs = file_refs(items, module, &Role::Lib, &resolver);
            let mut targets = refs.modules;
            for name in &refs.macro_uses {
                if let Some(defs) = graph.macro_defs.get(name) {
                    targets.extend(defs.iter().cloned());
                }
            }
            targets.remove(module);
            let mut implemented = refs.impl_self;
            implemented.remove(module);
            edges
                .entry(module.clone())
                .or_insert_with(BTreeSet::new)
                .extend(targets);
            impl_self
                .entry(module.clone())
                .or_insert_with(BTreeSet::new)
                .extend(implemented);
        }
        graph.edges = edges;
        graph.impl_self = impl_self;
        graph
    }

    /// Every module that transitively references one of `seeds`, plus the
    /// seeds.
    fn related(&self, seeds: &BTreeSet<Vec<String>>) -> BTreeSet<Vec<String>> {
        let mut reverse: HashMap<&Vec<String>, Vec<&Vec<String>>> = HashMap::new();
        for (from, tos) in &self.edges {
            for to in tos {
                reverse.entry(to).or_default().push(from);
            }
        }
        let mut related: BTreeSet<Vec<String>> = seeds.clone();
        let mut queue: VecDeque<Vec<String>> = seeds.iter().cloned().collect();
        while let Some(module) = queue.pop_front() {
            for from in reverse.get(&module).into_iter().flatten() {
                if related.insert((*from).clone()) {
                    queue.push_back((*from).clone());
                }
            }
        }
        related
    }
}

// ============================================================================
// Workspace
// ============================================================================

#[derive(Debug, Clone)]
struct Member {
    name: String,
    /// Absolute directory.
    dir: PathBuf,
    /// Names of workspace members this one depends on through path deps.
    deps: BTreeSet<String>,
}

struct Workspace {
    members: Vec<Member>,
    default_members: bool,
}

impl Workspace {
    /// Find the workspace containing `package` and read its members. A
    /// package outside any workspace is a workspace of one.
    fn load(package: &Package) -> Self {
        let single = || Workspace {
            members: vec![Member {
                name: package.name.clone(),
                dir: package.dir.clone(),
                deps: BTreeSet::new(),
            }],
            default_members: false,
        };
        let Some((root, manifest)) = find_workspace_root(&package.dir) else {
            return single();
        };
        let workspace = manifest.get("workspace");
        let patterns: Vec<String> = workspace
            .and_then(|w| w.get("members"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let excluded: HashSet<PathBuf> = workspace
            .and_then(|w| w.get("exclude"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|p| normalize(&root.join(p)))
                    .collect()
            })
            .unwrap_or_default();
        let default_members = workspace.and_then(|w| w.get("default-members")).is_some();
        let shared_deps = workspace.and_then(|w| w.get("dependencies"));

        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        if manifest.get("package").is_some() {
            dirs.insert(normalize(&root));
        }
        for pattern in &patterns {
            for dir in expand_member_pattern(&root, pattern) {
                if !excluded.contains(&dir) && dir.join("Cargo.toml").is_file() {
                    dirs.insert(dir);
                }
            }
        }
        dirs.insert(normalize(&package.dir));

        let mut members = Vec::new();
        let mut dir_names: HashMap<PathBuf, String> = HashMap::new();
        let mut raw_deps: Vec<Vec<PathBuf>> = Vec::new();
        for dir in dirs {
            let Some(manifest) = std::fs::read_to_string(dir.join("Cargo.toml"))
                .ok()
                .and_then(|s| toml::from_str::<Value>(&s).ok())
            else {
                continue;
            };
            let Some(name) = manifest
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            raw_deps.push(path_dependencies(&manifest, &dir, &root, shared_deps));
            dir_names.insert(dir.clone(), name.to_string());
            members.push(Member {
                name: name.to_string(),
                dir,
                deps: BTreeSet::new(),
            });
        }
        for (member, deps) in members.iter_mut().zip(raw_deps) {
            member.deps = deps
                .iter()
                .filter_map(|d| dir_names.get(d).cloned())
                .collect();
        }
        Workspace {
            members,
            default_members,
        }
    }

    fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }
}

fn find_workspace_root(package_dir: &Path) -> Option<(PathBuf, Value)> {
    let mut dir = Some(package_dir);
    while let Some(current) = dir {
        if let Some(manifest) = std::fs::read_to_string(current.join("Cargo.toml"))
            .ok()
            .and_then(|s| toml::from_str::<Value>(&s).ok())
        {
            if manifest.get("workspace").is_some() {
                return Some((current.to_path_buf(), manifest));
            }
        }
        if current.join(".git").exists() {
            break;
        }
        dir = current.parent();
    }
    None
}

/// Expand a `members` entry such as `crates/*` into directories.
fn expand_member_pattern(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for segment in Path::new(pattern).components() {
        let segment = segment.as_os_str().to_string_lossy().into_owned();
        let matcher = if segment.contains(['*', '?', '[']) {
            globset::Glob::new(&segment)
                .ok()
                .map(|g| g.compile_matcher())
        } else {
            None
        };
        let mut next = Vec::new();
        for dir in dirs {
            match &matcher {
                None => next.push(dir.join(&segment)),
                Some(matcher) => {
                    let Ok(entries) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        if entry.path().is_dir() && matcher.is_match(entry.file_name()) {
                            next.push(entry.path());
                        }
                    }
                }
            }
        }
        dirs = next;
    }
    dirs.into_iter().map(|d| normalize(&d)).collect()
}

/// Absolute directories of a manifest's path dependencies.
fn path_dependencies(
    manifest: &Value,
    dir: &Path,
    workspace_root: &Path,
    shared: Option<&Value>,
) -> Vec<PathBuf> {
    let mut tables: Vec<&Value> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(key) {
            tables.push(table);
        }
    }
    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(table) = target.get(key) {
                    tables.push(table);
                }
            }
        }
    }
    let mut deps = Vec::new();
    for table in tables {
        let Some(table) = table.as_table() else {
            continue;
        };
        for (name, spec) in table {
            if let Some(path) = spec.get("path").and_then(Value::as_str) {
                deps.push(normalize(&dir.join(path)));
            } else if spec.get("workspace").and_then(Value::as_bool) == Some(true) {
                let shared_name = spec.get("package").and_then(Value::as_str).unwrap_or(name);
                if let Some(path) = shared
                    .and_then(|s| s.get(shared_name).or_else(|| s.get(name)))
                    .and_then(|s| s.get("path"))
                    .and_then(Value::as_str)
                {
                    deps.push(normalize(&workspace_root.join(path)));
                }
            }
        }
    }
    deps
}

// ============================================================================
// The cargo test command
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// No package flag: the package in the working directory.
    Default,
    /// `--workspace` / `--all`, minus `--exclude`d packages.
    Workspace(Vec<String>),
    /// `-p` / `--package`.
    Packages(Vec<String>),
}

#[derive(Debug, Clone)]
struct CargoTest {
    /// Leading `NAME=value` assignments.
    env: Vec<String>,
    program: String,
    /// Arguments between the program and `test` (e.g. `+nightly`).
    pre: Vec<String>,
    sub: String,
    /// Flags kept on every narrowed invocation (`--locked`, features, …).
    common: Vec<String>,
    scope: Scope,
    all_targets: bool,
    /// Arguments after `--`, passed to the test binaries.
    trailing: Vec<String>,
}

const TARGET_FLAGS: &[&str] = &[
    "--lib",
    "--bins",
    "--bin",
    "--tests",
    "--test",
    "--examples",
    "--example",
    "--benches",
    "--bench",
    "--doc",
];
/// `cargo test` options whose value is a separate argument.
const CARGO_VALUE_FLAGS: &[&str] = &[
    "--features",
    "-F",
    "--profile",
    "--target",
    "--target-dir",
    "-j",
    "--jobs",
    "--color",
    "--message-format",
    "--config",
    "-Z",
    "--lockfile-path",
];
const LIBTEST_VALUE_FLAGS: &[&str] = &[
    "--test-threads",
    "--skip",
    "--format",
    "--color",
    "--logfile",
    "-Z",
    "--shuffle-seed",
    "--report-time",
];

impl CargoTest {
    fn parse(command: &str) -> Result<Self, String> {
        let parts =
            shell_words::split(command).map_err(|e| format!("cannot parse command: {e}"))?;
        let mut iter = parts.into_iter().peekable();
        let mut env = Vec::new();
        while let Some(part) = iter.peek() {
            let is_env = part.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
                    && !name.starts_with(|c: char| c.is_ascii_digit())
            });
            if !is_env {
                break;
            }
            env.push(iter.next().unwrap_or_default());
        }
        let program = iter.next().ok_or("empty command")?;
        let program_name = Path::new(&program)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if program_name != "cargo" {
            return Err("not a cargo test command".to_string());
        }
        let mut pre = Vec::new();
        let sub = loop {
            let part = iter.next().ok_or("not a cargo test command")?;
            if part == "test" || part == "t" {
                break part;
            }
            if part == "-C" || part.starts_with("--manifest-path") || !part.starts_with(['-', '+'])
            {
                return Err("not a cargo test command".to_string());
            }
            pre.push(part);
        };

        let mut common = Vec::new();
        let mut workspace = false;
        let mut excludes = Vec::new();
        let mut packages = Vec::new();
        let mut all_targets = false;
        let mut trailing = Vec::new();
        while let Some(part) = iter.next() {
            let flag = part.split('=').next().unwrap_or(&part).to_string();
            let value = |iter: &mut std::iter::Peekable<std::vec::IntoIter<String>>| {
                part.split_once('=')
                    .map(|(_, v)| v.to_string())
                    .or_else(|| {
                        part.strip_prefix("-p")
                            .filter(|v| !v.is_empty() && flag == part)
                            .map(str::to_string)
                    })
                    .or_else(|| iter.next())
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--" => {
                    trailing.extend(iter.by_ref());
                    break;
                }
                "--workspace" | "--all" => workspace = true,
                "--exclude" => excludes.push(value(&mut iter)?),
                "--package" => packages.push(value(&mut iter)?),
                _ if flag.starts_with("-p") => packages.push(value(&mut iter)?),
                "--all-targets" => all_targets = true,
                "--no-run" => return Err("the command only compiles tests (--no-run)".into()),
                "--manifest-path" => {
                    return Err("the command sets --manifest-path".into());
                }
                f if TARGET_FLAGS.contains(&f) => {
                    return Err(format!("the command already selects test targets ({f})"));
                }
                _ => {
                    let takes_value = common
                        .last()
                        .is_some_and(|p: &String| CARGO_VALUE_FLAGS.contains(&p.as_str()));
                    if !part.starts_with('-') && !takes_value {
                        return Err("the command already filters tests by name".into());
                    }
                    common.push(part);
                }
            }
        }
        let mut previous: Option<&str> = None;
        for arg in &trailing {
            if arg == "--exact" {
                return Err("the command passes --exact to the test harness".into());
            }
            let takes_value = previous.is_some_and(|p| LIBTEST_VALUE_FLAGS.contains(&p));
            if !arg.starts_with('-') && !takes_value {
                return Err("the command already filters tests by name".into());
            }
            previous = Some(arg);
        }
        if packages
            .iter()
            .chain(&excludes)
            .any(|p| p.contains(['*', '?', '[']))
        {
            return Err("the command selects packages by glob".into());
        }
        let scope = if workspace {
            Scope::Workspace(excludes)
        } else if !packages.is_empty() {
            Scope::Packages(packages)
        } else {
            Scope::Default
        };
        Ok(CargoTest {
            env,
            program,
            pre,
            sub,
            common,
            scope,
            all_targets,
            trailing,
        })
    }

    /// Render one invocation with extra selection flags and test filters.
    fn render(&self, selection: &[String], filters: &[String]) -> String {
        let mut parts: Vec<&str> = Vec::new();
        parts.extend(self.env.iter().map(String::as_str));
        parts.push(&self.program);
        parts.extend(self.pre.iter().map(String::as_str));
        parts.push(&self.sub);
        parts.extend(self.common.iter().map(String::as_str));
        parts.extend(selection.iter().map(String::as_str));
        if !self.trailing.is_empty() || !filters.is_empty() {
            parts.push("--");
            parts.extend(self.trailing.iter().map(String::as_str));
            parts.extend(filters.iter().map(String::as_str));
        }
        parts
            .iter()
            .map(|p| crate::executor::quote_command_argument(p))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ============================================================================
// Selection
// ============================================================================

const CARGO_CONFIG_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config",
    ".cargo/config.toml",
];

struct Selector<'a> {
    cargo: &'a CargoTest,
    package: &'a Package,
    workspace: &'a Workspace,
    explanation: Vec<String>,
}

/// What the changed files touch inside the package.
#[derive(Default)]
struct Touched {
    lib_seeds: BTreeSet<Vec<String>>,
    targets: BTreeSet<Owner>,
    members: BTreeSet<String>,
    full_reason: Option<String>,
}

impl Selector<'_> {
    fn select(mut self, files: &[PathBuf]) -> FilesListSelection {
        let in_scope = match self.scope_packages() {
            Ok(in_scope) => in_scope,
            Err(reason) => return full(vec![format!("full run: {reason}")]),
        };
        if !self.package.missing_roots.is_empty() {
            return full(vec![format!(
                "full run: cannot find the root file of {}",
                self.package.missing_roots.join(", ")
            )]);
        }
        let touched = self.classify(files);
        if let Some(reason) = touched.full_reason {
            self.explanation.push(format!("full run: {reason}"));
            return full(self.explanation);
        }

        let graph = LibGraph::build(self.package);
        let mut seeds = touched.lib_seeds.clone();
        for seed in &touched.lib_seeds {
            if let Some(implemented) = graph.impl_self.get(seed) {
                for module in implemented {
                    if seeds.insert(module.clone()) {
                        self.explanation.push(format!(
                            "module {} implements a type from {}; treating {} as changed",
                            module_name(seed),
                            module_name(module),
                            module_name(module)
                        ));
                    }
                }
            }
        }
        let related = graph.related(&seeds);
        let selected_targets = self.related_targets(&graph, &related, &touched.targets);

        // Workspace members that run in full.
        let root = self.package.name.clone();
        let root_touched = !related.is_empty() || !selected_targets.is_empty();
        let mut changed: BTreeSet<String> = touched.members.clone();
        let mut full_members = touched.members.clone();
        if root_touched {
            changed.insert(root.clone());
        }
        loop {
            let before = full_members.len();
            for member in &self.workspace.members {
                if member
                    .deps
                    .iter()
                    .any(|d| changed.contains(d) || full_members.contains(d))
                    && full_members.insert(member.name.clone())
                {
                    self.explanation.push(format!(
                        "package {} depends on a changed package; running it in full",
                        member.name
                    ));
                }
            }
            if full_members.len() == before {
                break;
            }
        }

        // Library filters.
        let lib_modules = graph.modules.len().saturating_sub(1);
        let related_modules: Vec<&Vec<String>> = related.iter().filter(|m| !m.is_empty()).collect();
        let filters = compact_filters(&related);
        let mut root_full = full_members.contains(&root);
        if !root_full
            && lib_modules >= FULL_RUN_MIN_MODULES
            && related_modules.len() as f64 >= lib_modules as f64 * FULL_RUN_SHARE
        {
            self.explanation.push(format!(
                "full run of {root}: {} of {lib_modules} library modules are related",
                related_modules.len()
            ));
            root_full = true;
        } else if !root_full && filters.len() > MAX_FILTERS {
            self.explanation.push(format!(
                "full run of {root}: {} module filters exceed the limit of {MAX_FILTERS}",
                filters.len()
            ));
            root_full = true;
        }
        if root_full {
            full_members.insert(root.clone());
        }

        let full_in_scope: Vec<String> = in_scope
            .iter()
            .filter(|p| full_members.contains(*p))
            .cloned()
            .collect();
        if !in_scope.is_empty() && full_in_scope.len() == in_scope.len() {
            self.explanation
                .push("full run: every package in scope is affected".to_string());
            return full(self.explanation);
        }

        let mut commands = Vec::new();
        let root_in_scope = in_scope.contains(&root);
        if root_in_scope && !root_full {
            if !related_modules.is_empty() {
                self.explain_list(
                    &format!("related library modules ({})", related_modules.len()),
                    related_modules.iter().map(|m| module_name(m)),
                );
            }
            let pkg = ["-p".to_string(), root.clone()];
            if !filters.is_empty() {
                let mut selection = pkg.to_vec();
                selection.push("--lib".to_string());
                commands.push(self.cargo.render(&selection, &filters));
            }
            // Tests in the crate root have no module prefix to filter on, so
            // they run by exact name whenever any library module is related.
            let root_tests = self.root_test_names();
            if !related.is_empty() && !root_tests.is_empty() {
                self.explain_list(
                    &format!("crate root tests ({})", root_tests.len()),
                    root_tests.iter().cloned(),
                );
                let mut selection = pkg.to_vec();
                selection.push("--lib".to_string());
                let mut filters = vec!["--exact".to_string()];
                filters.extend(root_tests);
                commands.push(self.cargo.render(&selection, &filters));
            }
            if related.is_empty() {
                if self.package.lib.is_some() {
                    self.explanation
                        .push("library unit tests and doctests: none related".to_string());
                }
            } else if self.cargo.all_targets {
                self.explanation.push(
                    "doctests: not run (the original command uses --all-targets, which excludes them)"
                        .to_string(),
                );
            } else {
                // Doc comments are not analysed, so any doctest may reach a
                // related module.
                let mut selection = pkg.to_vec();
                selection.push("--doc".to_string());
                commands.push(self.cargo.render(&selection, &[]));
                self.explanation
                    .push("doctests: all run (doc comments are not analysed)".to_string());
            }
            let mut selection = pkg.to_vec();
            let mut names = Vec::new();
            if selected_targets.iter().any(|o| matches!(o, Owner::Bin(_))) {
                selection.push("--bins".to_string());
                names.push("bins".to_string());
            }
            for owner in &selected_targets {
                if let Owner::Aux(kind, name) = owner {
                    if *kind == AuxKind::Bench && !self.cargo.all_targets {
                        continue;
                    }
                    selection.push(kind.flag().to_string());
                    selection.push(name.clone());
                    names.push(owner.to_string());
                }
            }
            if !names.is_empty() {
                self.explain_list(
                    &format!("selected targets ({})", names.len()),
                    names.into_iter(),
                );
                commands.push(self.cargo.render(&selection, &[]));
            }
        }
        if !full_in_scope.is_empty() {
            let mut selection = Vec::new();
            for package in &full_in_scope {
                selection.push("-p".to_string());
                selection.push(package.clone());
            }
            if self.cargo.all_targets {
                selection.push("--all-targets".to_string());
            }
            self.explanation
                .push(format!("full run of {}", full_in_scope.join(", ")));
            commands.push(self.cargo.render(&selection, &[]));
        }
        if commands.is_empty() {
            // Files changed but none mapped to a test. Run everything rather
            // than report a skip.
            self.explanation.push(
                "full run: no test maps to the changed files, so nothing can be ruled out"
                    .to_string(),
            );
            return full(self.explanation);
        }
        FilesListSelection {
            plan: FilesListPlan::Commands(commands),
            explanation: self.explanation,
        }
    }

    /// Full names of the `#[test]` functions in the library root file.
    fn root_test_names(&self) -> Vec<String> {
        let Some(lib) = &self.package.lib else {
            return Vec::new();
        };
        let Some((root, _)) = lib.files.iter().find(|(_, m)| m.is_empty()) else {
            return Vec::new();
        };
        self.package
            .items
            .get(root)
            .into_iter()
            .flatten()
            .filter_map(|item| match item {
                Item::TestFn { inline, name } => {
                    let mut path = inline.clone();
                    path.push(name.clone());
                    Some(path.join("::"))
                }
                _ => None,
            })
            .collect()
    }

    fn explain_list(&mut self, title: &str, items: impl Iterator<Item = String>) {
        let items: Vec<String> = items.collect();
        let shown: Vec<&str> = items
            .iter()
            .take(EXPLAIN_LIST_LIMIT)
            .map(String::as_str)
            .collect();
        let more = items.len().saturating_sub(shown.len());
        let suffix = if more > 0 {
            format!(", … and {more} more")
        } else {
            String::new()
        };
        self.explanation
            .push(format!("{title}: {}{suffix}", shown.join(", ")));
    }

    /// Packages the original command tests.
    fn scope_packages(&self) -> Result<Vec<String>, String> {
        match &self.cargo.scope {
            Scope::Default => {
                if self.workspace.default_members
                    && find_workspace_root(&self.package.dir)
                        .is_some_and(|(root, _)| normalize(&root) == normalize(&self.package.dir))
                {
                    return Err("the workspace sets default-members".into());
                }
                Ok(vec![self.package.name.clone()])
            }
            Scope::Workspace(excludes) => Ok(self
                .workspace
                .members
                .iter()
                .map(|m| m.name.clone())
                .filter(|n| !excludes.contains(n))
                .collect()),
            Scope::Packages(names) => {
                for name in names {
                    if self.workspace.member(name).is_none() {
                        return Err(format!("package {name} is not a workspace member"));
                    }
                }
                Ok(names.clone())
            }
        }
    }

    /// The workspace member (other than this package) owning `abs`, if any.
    fn nested_member(&self, abs: &Path) -> Option<&Member> {
        self.workspace
            .members
            .iter()
            .filter(|m| m.dir != self.package.dir && abs.starts_with(&m.dir))
            .max_by_key(|m| m.dir.components().count())
    }

    /// Map each changed file onto the package's targets and modules.
    fn classify(&mut self, files: &[PathBuf]) -> Touched {
        let mut touched = Touched::default();
        let lib_root = self
            .package
            .lib
            .as_ref()
            .and_then(|lib| lib.files.iter().find(|(_, m)| m.is_empty()))
            .map(|(f, _)| f.clone());
        for file in files {
            let file = normalize(file);
            let shown = file.to_string_lossy().replace('\\', "/");
            let abs = self.package.dir.join(&file);
            if let Some(member) = self.nested_member(&abs).map(|m| m.name.clone()) {
                self.explanation
                    .push(format!("{shown}: in workspace member {member}"));
                touched.members.insert(member);
                continue;
            }
            if CARGO_CONFIG_FILES.iter().any(|c| Path::new(c) == file) {
                touched.full_reason = Some(format!("{shown} changed"));
                return touched;
            }
            if Some(&file) == lib_root.as_ref() {
                touched.full_reason = Some(format!("{shown} is the library root"));
                return touched;
            }
            if let Some(owners) = self.package.owners.get(&file) {
                for owner in owners {
                    if *owner == Owner::Build {
                        touched.full_reason = Some(format!("{shown} is part of the build script"));
                        return touched;
                    }
                    self.explanation.push(format!("{shown}: {owner}"));
                    self.touch(&mut touched, owner.clone());
                }
                continue;
            }
            if file.extension().is_some_and(|e| e == "rs") {
                touched.full_reason = Some(format!("{shown} is not part of any crate target"));
                return touched;
            }
            // A non-Rust file: find the sources that name it.
            let mentions = self.mentions(&file);
            if mentions.is_empty() {
                touched.full_reason = Some(format!(
                    "{shown} is not a Rust source and no source names it"
                ));
                return touched;
            }
            let mut described = Vec::new();
            for mention in mentions {
                match mention {
                    Mention::Owner(Owner::Build) => {
                        touched.full_reason = Some(format!("{shown} is named by the build script"));
                        return touched;
                    }
                    Mention::Owner(Owner::Lib(module)) if module.is_empty() => {
                        touched.full_reason = Some(format!("{shown} is named by the library root"));
                        return touched;
                    }
                    Mention::Owner(owner) => {
                        described.push(owner.to_string());
                        self.touch(&mut touched, owner);
                    }
                    Mention::Member(name) => {
                        described.push(format!("package {name}"));
                        touched.members.insert(name);
                    }
                }
            }
            described.sort();
            described.dedup();
            self.explanation
                .push(format!("{shown}: named by {}", described.join(", ")));
        }
        touched
    }

    fn touch(&self, touched: &mut Touched, owner: Owner) {
        match owner {
            Owner::Lib(module) => {
                touched.lib_seeds.insert(module);
            }
            other => {
                touched.targets.insert(other);
            }
        }
    }

    /// Sources whose string literals name `file` or one of its directories.
    fn mentions(&self, file: &Path) -> BTreeSet<Mention> {
        let segments: Vec<String> = file
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let Some((basename, dirs)) = segments.split_last() else {
            return BTreeSet::new();
        };
        let dir_names: Vec<&String> = dirs
            .iter()
            .enumerate()
            .filter(|(i, d)| {
                !(*i == 0 && matches!(d.as_str(), "src" | "tests" | "benches" | "examples"))
            })
            .map(|(_, d)| d)
            .collect();
        // A bare file name only counts in a source beside the file
        // (`include_str!("data.json")`); elsewhere it must be part of a path.
        let file_dir = normalize(&self.package.dir.join(file))
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        // A bare directory name counts when it is passed to a path call
        // (`dir.join("fixtures")`).
        let names = |s: &str, path_arg: bool, source: &Path| {
            let sibling = path_arg || source.parent() == Some(file_dir.as_path());
            mentions_name(s, basename, sibling)
                || dir_names
                    .iter()
                    .any(|d| mentions_dir(s, d) || (path_arg && s == d.as_str()))
        };

        let mut found = BTreeSet::new();
        for (path, items) in &self.package.items {
            let source = normalize(&self.package.dir.join(path));
            if items
                .iter()
                .any(|item| matches!(item, Item::Str(s, path_arg) if names(s, *path_arg, &source)))
            {
                for owner in self.package.owners.get(path).into_iter().flatten() {
                    found.insert(Mention::Owner(owner.clone()));
                }
            }
        }
        for member in &self.workspace.members {
            if member.dir == self.package.dir {
                continue;
            }
            for source in rust_sources(&member.dir) {
                let Ok(src) = std::fs::read_to_string(&source) else {
                    continue;
                };
                if extract(&lex(&src)).iter().any(
                    |item| matches!(item, Item::Str(s, path_arg) if names(s, *path_arg, &source)),
                ) {
                    found.insert(Mention::Member(member.name.clone()));
                    break;
                }
            }
        }
        found
    }

    /// Bins, tests, benches and examples that a change reaches.
    fn related_targets(
        &self,
        graph: &LibGraph,
        related: &BTreeSet<Vec<String>>,
        touched: &BTreeSet<Owner>,
    ) -> BTreeSet<Owner> {
        let resolver = Resolver {
            modules: &graph.modules,
            lib_name: &self.package.lib_name,
        };
        let reaches_related = |target: &CrateTarget| {
            target.files.keys().any(|file| {
                let items = self
                    .package
                    .items
                    .get(file)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let refs = file_refs(items, &[], &Role::External, &resolver);
                refs.modules.iter().any(|m| related.contains(m))
                    || refs.macro_uses.iter().any(|name| {
                        graph
                            .macro_defs
                            .get(name)
                            .is_some_and(|defs| defs.iter().any(|d| related.contains(d)))
                    })
            })
        };
        let mut selected: BTreeSet<Owner> = touched.clone();
        for target in &self.package.others {
            if matches!(target.owner, Owner::Bin(_)) && reaches_related(target) {
                selected.insert(target.owner.clone());
            }
        }
        let bins: BTreeSet<String> = selected
            .iter()
            .filter_map(|o| match o {
                Owner::Bin(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        for target in &self.package.others {
            if !matches!(target.owner, Owner::Aux(..)) || selected.contains(&target.owner) {
                continue;
            }
            // Any sign that the test runs one of the package's binaries links
            // it to every related binary; names are not matched.
            let runs_related_bin = !bins.is_empty()
                && target.files.keys().any(|file| {
                    self.package
                        .items
                        .get(file)
                        .into_iter()
                        .flatten()
                        .any(spawns_package_binary)
                });
            if runs_related_bin || reaches_related(target) {
                selected.insert(target.owner.clone());
            }
        }
        selected
    }
}

/// Whether an item shows a test running the package's binaries:
/// `env!("CARGO_BIN_EXE_…")`, assert_cmd's `cargo_bin`, or escargot.
fn spawns_package_binary(item: &Item) -> bool {
    const MARKERS: &[&str] = &[
        "cargo_bin",
        "cargo_bin_cmd",
        "assert_cmd",
        "escargot",
        "CargoBuild",
    ];
    match item {
        Item::Str(s, _) => s.contains("CARGO_BIN_"),
        Item::Path { raw, .. } => raw.segs.iter().any(|seg| MARKERS.contains(&seg.as_str())),
        Item::MacroUse(name) => MARKERS.contains(&name.as_str()),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Mention {
    Owner(Owner),
    Member(String),
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_separator(c: Option<char>) -> bool {
    matches!(c, Some('/') | Some('\\'))
}

/// Whether string `s` names a file called `name` (not part of a longer
/// name). Unless the source sits beside the file, the name must follow a
/// path separator: a bare `"AGENTS.md"` is usually a generic name, not this
/// file.
fn mentions_name(s: &str, name: &str, sibling: bool) -> bool {
    s.match_indices(name).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + name.len()..].chars().next();
        !before.is_some_and(is_name_char)
            && !after.is_some_and(is_name_char)
            && (sibling || is_separator(before))
    })
}

/// Whether string `s` names directory `dir` as a whole segment of a path
/// (`"fixtures/"`, `"../port"`), not as a bare word.
fn mentions_dir(s: &str, dir: &str) -> bool {
    s.match_indices(dir).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + dir.len()..].chars().next();
        !before.is_some_and(is_name_char)
            && (after.is_none() || is_separator(after))
            && (is_separator(before) || is_separator(after))
    })
}

/// Every `.rs` file under `dir`, skipping build output.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target" && e.file_name() != ".git")
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "rs"))
        .map(|e| e.into_path())
        .collect()
}

/// `a::b::` test-name filters for related modules, dropping modules whose
/// ancestor is already selected (libtest filters match substrings).
fn compact_filters(related: &BTreeSet<Vec<String>>) -> Vec<String> {
    let mut filters = Vec::new();
    for module in related {
        if module.is_empty() {
            continue;
        }
        let covered = (1..module.len()).any(|len| related.contains(&module[..len]));
        if !covered {
            filters.push(format!("{}::", module.join("::")));
        }
    }
    filters
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, path: &str, content: &str) {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A crate where `a` uses `b` and `b` uses `c`, plus an unrelated `d`
    /// and integration tests that reach the library at different depths.
    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write(
            dir,
            "Cargo.toml",
            "[package]\nname = \"demo-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            dir,
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
        );
        write(
            dir,
            "src/a.rs",
            "use crate::b::helper;\npub fn a() -> u8 { helper() }\n#[cfg(test)]\nmod tests { use super::*; #[test] fn t() { assert_eq!(a(), 3); } }\n",
        );
        write(
            dir,
            "src/b/mod.rs",
            "pub mod inner;\npub fn helper() -> u8 { super::c::value() + inner::one() }\n",
        );
        write(dir, "src/b/inner.rs", "pub fn one() -> u8 { 1 }\n");
        write(
            dir,
            "src/c.rs",
            "pub fn value() -> u8 { 2 }\npub fn fixture() -> &'static str { include_str!(\"../fixtures/data.json\") }\n",
        );
        write(dir, "src/d.rs", "pub fn d() {}\n");
        write(dir, "fixtures/data.json", "{}\n");
        write(dir, "README.md", "# demo\n");
        write(
            dir,
            "tests/uses_a.rs",
            "use demo_crate::a::a;\n#[test]\nfn it() { assert_eq!(a(), 3); }\n",
        );
        write(
            dir,
            "tests/uses_d.rs",
            "#[test]\nfn it() { demo_crate::d::d(); }\n",
        );
        write(
            dir,
            "tests/shared.rs",
            "#[path = \"support/mod.rs\"]\nmod support;\n#[test]\nfn it() { support::go(); }\n",
        );
        write(dir, "tests/support/mod.rs", "pub fn go() {}\n");
        tmp
    }

    fn run(tmp: &TempDir, command: &str, files: &[&str]) -> FilesListSelection {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        select(tmp.path(), command, &files)
    }

    fn commands(selection: &FilesListSelection) -> Vec<String> {
        match &selection.plan {
            FilesListPlan::Commands(commands) => commands.clone(),
            other => panic!(
                "expected commands, got {other:?}: {:#?}",
                selection.explanation
            ),
        }
    }

    #[test]
    fn changing_a_leaf_module_selects_every_module_that_reaches_it() {
        let tmp = fixture();
        let selection = run(&tmp, "cargo test --locked", &["src/c.rs"]);
        let commands = commands(&selection);
        assert_eq!(
            commands[0],
            "cargo test --locked -p demo-crate --lib -- a:: b:: c::"
        );
        assert_eq!(commands[1], "cargo test --locked -p demo-crate --doc");
        // uses_a reaches c through a → b; uses_d and shared do not.
        assert_eq!(
            commands[2],
            "cargo test --locked -p demo-crate --test uses_a"
        );
        assert_eq!(commands.len(), 3);
    }

    #[test]
    fn changing_a_top_module_selects_only_that_module() {
        let tmp = fixture();
        let selection = run(&tmp, "cargo test --all-targets", &["src/a.rs"]);
        let commands = commands(&selection);
        assert_eq!(
            commands,
            vec![
                "cargo test -p demo-crate --lib -- a::".to_string(),
                "cargo test -p demo-crate --test uses_a".to_string(),
            ]
        );
        assert!(selection
            .explanation
            .iter()
            .any(|line| line.contains("doctests: not run")));
    }

    #[test]
    fn a_child_module_reached_through_its_parent_selects_the_parent() {
        let tmp = fixture();
        let selection = run(&tmp, "cargo test --all-targets", &["src/b/inner.rs"]);
        // b::inner:: is covered by the b:: filter.
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- a:: b::"
        );
    }

    #[test]
    fn manifest_changes_run_the_original_command() {
        let tmp = fixture();
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "src/lib.rs",
            "rust-toolchain.toml",
        ] {
            let selection = run(&tmp, "cargo test", &[file]);
            assert_eq!(selection.plan, FilesListPlan::Full, "{file}");
        }
    }

    #[test]
    fn rust_files_outside_every_target_run_the_original_command() {
        let tmp = fixture();
        write(tmp.path(), "src/orphan.rs", "pub fn x() {}\n");
        let selection = run(&tmp, "cargo test", &["src/orphan.rs"]);
        assert_eq!(selection.plan, FilesListPlan::Full);
    }

    #[test]
    fn integration_tests_run_when_they_change_or_their_support_changes() {
        let tmp = fixture();
        let selection = run(
            &tmp,
            "cargo test --all-targets",
            &["tests/support/mod.rs", "tests/uses_d.rs"],
        );
        assert_eq!(
            commands(&selection),
            vec!["cargo test -p demo-crate --test shared --test uses_d".to_string()]
        );
    }

    #[test]
    fn data_files_map_to_the_sources_that_name_them() {
        let tmp = fixture();
        let selection = run(&tmp, "cargo test --all-targets", &["fixtures/data.json"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- a:: b:: c::"
        );

        // A file no source names runs the original command.
        let selection = run(&tmp, "cargo test --all-targets", &["README.md"]);
        assert_eq!(selection.plan, FilesListPlan::Full);
    }

    #[test]
    fn commands_that_already_select_targets_run_unchanged() {
        let tmp = fixture();
        for command in [
            "cargo test --lib",
            "cargo test -- some_test",
            "cargo test --no-run",
            "cargo build",
            "make test",
        ] {
            let selection = run(&tmp, command, &["src/a.rs"]);
            assert_eq!(selection.plan, FilesListPlan::Full, "{command}");
        }
    }

    #[test]
    fn trailing_harness_flags_are_kept() {
        let tmp = fixture();
        let selection = run(&tmp, "cargo test -- --test-threads 1", &["src/a.rs"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- --test-threads 1 a::"
        );
    }

    #[test]
    fn workspace_dependents_of_a_changed_crate_run_in_full() {
        let tmp = fixture();
        let dir = tmp.path();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\".\", \"crates/*\"]\n\n[package]\nname = \"demo-crate\"\nversion = \"0.1.0\"\n\n[dependencies]\nhelper = { path = \"crates/helper\" }\n",
        )
        .unwrap();
        write(
            dir,
            "crates/helper/Cargo.toml",
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\n",
        );
        write(dir, "crates/helper/src/lib.rs", "pub fn h() {}\n");
        write(
            dir,
            "crates/user/Cargo.toml",
            "[package]\nname = \"user\"\nversion = \"0.1.0\"\n\n[dependencies]\ndemo-crate = { path = \"../..\" }\n",
        );
        write(dir, "crates/user/src/lib.rs", "pub fn u() {}\n");
        write(
            dir,
            "crates/lone/Cargo.toml",
            "[package]\nname = \"lone\"\nversion = \"0.1.0\"\n",
        );
        write(dir, "crates/lone/src/lib.rs", "pub fn l() {}\n");

        // A root module change: the root narrows, `user` (depends on the
        // root) runs in full, `helper` and `lone` do not run.
        let selection = run(
            &tmp,
            "cargo test --locked --workspace --all-targets",
            &["src/d.rs"],
        );
        assert_eq!(
            commands(&selection),
            vec![
                "cargo test --locked -p demo-crate --lib -- d::".to_string(),
                "cargo test --locked -p demo-crate --test uses_d".to_string(),
                "cargo test --locked -p user --all-targets".to_string(),
            ]
        );

        // A change in `lone` runs only `lone`.
        let selection = run(&tmp, "cargo test --workspace", &["crates/lone/src/lib.rs"]);
        assert_eq!(commands(&selection), vec!["cargo test -p lone".to_string()]);

        // A change in `helper` runs it and everything above it, which here
        // is all but `lone`.
        let selection = run(
            &tmp,
            "cargo test --workspace",
            &["crates/helper/src/lib.rs"],
        );
        assert_eq!(
            commands(&selection),
            vec!["cargo test -p demo-crate -p helper -p user".to_string()]
        );
    }

    #[test]
    fn impl_of_a_foreign_type_seeds_the_type_module() {
        let tmp = fixture();
        write(
            tmp.path(),
            "src/d.rs",
            "use crate::b::inner::Thing;\nimpl Thing { pub fn extra(&self) {} }\n",
        );
        write(
            tmp.path(),
            "src/b/inner.rs",
            "pub struct Thing;\npub fn one() -> u8 { 1 }\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["src/d.rs"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- a:: b:: d::"
        );
    }

    #[test]
    fn macros_link_their_callers() {
        let tmp = fixture();
        write(
            tmp.path(),
            "src/d.rs",
            "#[macro_export]\nmacro_rules! shout { () => { 1 } }\n",
        );
        write(
            tmp.path(),
            "src/c.rs",
            "pub fn value() -> u8 { shout!() }\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["src/d.rs"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- a:: b:: c:: d::"
        );
    }

    #[test]
    fn crate_root_tests_run_by_exact_name_when_the_root_is_related() {
        let tmp = fixture();
        write(
            tmp.path(),
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn root_uses_b() { assert!(super::b::helper() > 0); }\n    #[tokio::test]\n    async fn root_async() {}\n}\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["src/c.rs"]);
        assert_eq!(
            commands(&selection)[1],
            "cargo test -p demo-crate --lib -- --exact tests::root_uses_b tests::root_async"
        );
        // Any related module brings in the root's tests.
        let selection = run(&tmp, "cargo test --all-targets", &["src/d.rs"]);
        assert!(commands(&selection).iter().any(|c| c.contains("--exact")));
    }

    #[test]
    fn declared_bins_without_a_path_follow_cargo_inference() {
        let tmp = fixture();
        let dir = tmp.path();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo-crate\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"demo-crate\"\n",
        )
        .unwrap();
        write(dir, "src/main.rs", "fn main() { demo_crate::d::d(); }\n");
        write(
            dir,
            "tests/cli.rs",
            "#[test]\nfn runs() { let _ = env!(\"CARGO_BIN_EXE_demo-crate\"); }\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["src/d.rs"]);
        assert_eq!(
            commands(&selection)[1],
            "cargo test -p demo-crate --bins --test cli --test uses_d"
        );

        // A declared target whose file does not exist cannot be analysed.
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo-crate\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"gone\"\n",
        )
        .unwrap();
        let selection = run(&tmp, "cargo test --all-targets", &["src/d.rs"]);
        assert_eq!(selection.plan, FilesListPlan::Full);
    }

    #[test]
    fn unnamed_files_beside_sources_run_in_full() {
        let tmp = fixture();
        write(tmp.path(), "tests/golden/one.json", "{}\n");
        let selection = run(&tmp, "cargo test --all-targets", &["tests/golden/one.json"]);
        assert_eq!(selection.plan, FilesListPlan::Full);

        // A path call with the bare directory name counts as naming it.
        write(
            tmp.path(),
            "tests/uses_d.rs",
            "#[test]\nfn it() { let _ = std::path::Path::new(\"tests\").join(\"golden\"); }\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["tests/golden/one.json"]);
        assert_eq!(
            commands(&selection),
            vec!["cargo test -p demo-crate --test uses_d".to_string()]
        );
    }

    #[test]
    fn tests_that_run_the_binary_through_assert_cmd_follow_it() {
        let tmp = fixture();
        write(tmp.path(), "src/main.rs", "fn main() {}\n");
        write(
            tmp.path(),
            "tests/cli.rs",
            "use assert_cmd::Command;\n#[test]\nfn runs() { Command::cargo_bin(\"demo\").unwrap(); }\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["src/main.rs"]);
        assert_eq!(
            commands(&selection),
            vec!["cargo test -p demo-crate --bins --test cli".to_string()]
        );
    }

    #[test]
    fn blanket_impls_seed_the_trait_module() {
        let tmp = fixture();
        write(
            tmp.path(),
            "src/b/inner.rs",
            "pub trait Ext {}\npub fn one() -> u8 { 1 }\n",
        );
        write(
            tmp.path(),
            "src/d.rs",
            "impl<T: Clone> crate::b::inner::Ext for T {}\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["src/d.rs"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- a:: b:: d::"
        );
    }

    #[test]
    fn positional_test_names_in_the_command_run_unchanged() {
        let tmp = fixture();
        let selection = run(&tmp, "cargo test --features fast some_test", &["src/a.rs"]);
        assert_eq!(selection.plan, FilesListPlan::Full);
        let selection = run(&tmp, "cargo test --features fast", &["src/a.rs"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test --features fast -p demo-crate --lib -- a::"
        );
    }

    #[test]
    fn lexer_ignores_comments_and_keeps_strings() {
        let toks = lex(
            "// crate::a\n/* crate::b /* nested */ */ let s = r#\"x\"y\"#; 'a'; '\\''; &'b str",
        );
        assert!(!toks.contains(&Tok::Ident("crate".into())));
        assert!(toks.contains(&Tok::Str("x\"y".into())));
    }

    #[test]
    fn use_trees_expand_groups_aliases_and_globs() {
        let mut out = Vec::new();
        let toks = lex("crate::{a::{b, c as d}, e::*, self}");
        parse_tree(&toks, 0, &[], &mut out);
        let leaves: Vec<(String, bool, Option<String>)> = out
            .iter()
            .map(|p| (p.segs.join("::"), p.glob, p.alias.clone()))
            .collect();
        assert_eq!(
            leaves,
            vec![
                ("crate::a::b".to_string(), false, None),
                ("crate::a::c".to_string(), false, Some("d".to_string())),
                ("crate::e".to_string(), true, None),
                ("crate".to_string(), false, None),
            ]
        );
    }

    #[test]
    fn mention_matching_respects_name_boundaries() {
        assert!(mentions_name("../port/index.toml", "index.toml", false));
        assert!(mentions_name("port/index.toml: bad", "index.toml", false));
        assert!(!mentions_name("myindex.toml", "index.toml", true));
        assert!(!mentions_name("AGENTS.md", "AGENTS.md", false));
        assert!(mentions_name("AGENTS.md", "AGENTS.md", true));
        assert!(mentions_dir("services/rust/archdev/port", "port"));
        assert!(mentions_dir("../../port/", "port"));
        assert!(mentions_dir("port/index.toml", "port"));
        assert!(!mentions_dir("port", "port"));
        assert!(!mentions_dir("port util", "port"));
        assert!(!mentions_dir("report/", "port"));
    }

    #[test]
    fn generic_file_names_do_not_count_as_mentions() {
        let tmp = fixture();
        write(
            tmp.path(),
            "src/d.rs",
            "pub const NAME: &str = \"README.md\";\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["README.md"]);
        assert_eq!(selection.plan, FilesListPlan::Full);

        write(
            tmp.path(),
            "src/d.rs",
            "pub const DOC: &str = include_str!(\"../README.md\");\n",
        );
        let selection = run(&tmp, "cargo test --all-targets", &["README.md"]);
        assert_eq!(
            commands(&selection)[0],
            "cargo test -p demo-crate --lib -- d::"
        );
    }
}
