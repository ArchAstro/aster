//! Rust facts: items, `impl` and trait methods, `use` bindings, paths and
//! `#[test]` functions, including the ones in a `#[cfg(test)]` module beside
//! the code they test.
//!
//! Paths are kept as written, made absolute only as far as the file alone
//! allows (`use` bindings, `super` out of an inline module). The index knows
//! the crate's module tree and resolves the rest.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner, ANY_TYPE,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

/// A name bound by `use`: `local` stands for the item at `path`.
struct Binding {
    local: String,
    path: Vec<String>,
}

/// What is known of the type of a value.
#[derive(Clone, PartialEq)]
enum Typing {
    Unknown,
    /// A type the code writes out.
    Known(String),
    /// Whatever calling `path` returns, after methods that keep the type.
    Result {
        path: Vec<String>,
        through: Vec<String>,
    },
}

/// Methods of the standard library that give back the value they are
/// called on, or what it wraps. The index checks that the workspace does
/// not give any of these names another meaning.
const KEEPS_TYPE: &[&str] = &[
    "unwrap",
    "expect",
    "clone",
    "cloned",
    "copied",
    "as_ref",
    "as_mut",
    "as_deref",
    "borrow",
    "borrow_mut",
    "lock",
    "read",
    "write",
    "into_inner",
    "to_owned",
    "unwrap_or_default",
];
/// Constructors that wrap their argument without changing what it is.
const WRAPS: &[&[&str]] = &[
    &["Arc", "new"],
    &["Rc", "new"],
    &["Box", "new"],
    &["Box", "pin"],
    &["Mutex", "new"],
    &["RwLock", "new"],
    &["RefCell", "new"],
    &["Cell", "new"],
    &["Some"],
    &["Ok"],
];

/// Wrappers a method call sees through to the type inside.
const POINTERS: &[&str] = &["Box", "Rc", "Arc"];
/// Signs that a test runs the package's own binary.
const BINARY_MARKERS: &[&str] = &[
    "cargo_bin",
    "cargo_bin_cmd",
    "assert_cmd",
    "escargot",
    "CargoBuild",
];

struct Walker<'a> {
    source: &'a str,
    facts: FileFacts,
    inert: Vec<(usize, usize)>,
    bindings: Vec<Binding>,
    /// Paths each definition mentions, before bindings are applied.
    paths: Vec<(Owner, Vec<String>)>,
    /// Inline modules enclosing the item being read.
    modules: Vec<String>,
    /// Inside `#[cfg(test)]`.
    in_test: bool,
    /// The type `self` and `Self` stand for.
    self_type: Option<String>,
    /// Inside a trait, where `Self` is whatever implements it.
    in_trait: bool,
    /// Type parameters in scope.
    type_params: HashSet<String>,
    /// Names the function being read binds, with the type of each when it
    /// is written out.
    locals: HashMap<String, Typing>,
    /// Member calls on the result of a call, before bindings are applied.
    results: Vec<(Owner, Vec<String>, Vec<String>, String)>,
    /// The declared type of each field of the structs in this file, by
    /// `(struct, field)`.
    fields: HashMap<(String, String), Option<String>>,
}

pub fn extract(_path: &Path, source: &str) -> FileFacts {
    let mut facts = FileFacts::new(Family::Rust);
    facts.qualified = true;
    facts.members = true;
    facts.loads_by_import = true;
    facts.types_by_import = true;
    facts.macros_by_name = true;
    facts.static_types = true;
    let Some(tree) = parse(tree_sitter_rust::LANGUAGE.into(), source) else {
        facts.parse_error = true;
        return facts;
    };
    facts.parse_error = tree.root_node().has_error();
    let mut walker = Walker {
        source,
        facts,
        inert: Vec::new(),
        bindings: Vec::new(),
        paths: Vec::new(),
        modules: Vec::new(),
        in_test: false,
        self_type: None,
        in_trait: false,
        type_params: HashSet::new(),
        locals: HashMap::new(),
        results: Vec::new(),
        fields: HashMap::new(),
    };
    walker.struct_fields(tree.root_node());
    walker.items(tree.root_node(), None);
    walker.apply_bindings();
    let mut facts = walker.facts;
    facts.classify_lines(source, &walker.inert);
    facts
}

/// What the attributes before an item say about it.
#[derive(Default, Clone)]
struct Attributes<'t> {
    /// The attributes themselves: a derive or an attribute macro is code
    /// the item uses.
    nodes: Vec<Node<'t>>,
    /// Marks a function a procedural macro, run by the compiler.
    proc_macro: bool,
    /// First line of the attributes and doc comments.
    start: Option<usize>,
    test: bool,
    cfg_test: bool,
}

fn is_doc(comment: &str) -> bool {
    (comment.starts_with("///") && !comment.starts_with("////"))
        || comment.starts_with("//!")
        || (comment.starts_with("/**") && !comment.starts_with("/***") && comment != "/**/")
        || comment.starts_with("/*!")
}

impl Walker<'_> {
    /// The items of a file, module, `impl` or trait body. `method_of` names
    /// the type whose methods the functions here are.
    fn items(&mut self, body: Node, method_of: Option<&str>) {
        let mut attributes = Attributes::default();
        for item in children(body) {
            match item.kind() {
                "line_comment" | "block_comment" => {
                    // Documentation holds doctests, so it is part of the
                    // item it describes.
                    if is_doc(text(item, self.source)) {
                        attributes.start.get_or_insert(span(item).0);
                    } else {
                        self.inert.push((item.start_byte(), item.end_byte()));
                    }
                    continue;
                }
                "attribute_item" => {
                    attributes.start.get_or_insert(span(item).0);
                    self.attribute(item, &mut attributes);
                    attributes.nodes.push(item);
                    continue;
                }
                "inner_attribute_item" => {
                    let inner = text(item, self.source);
                    if inner.contains("cfg(test)") && !inner.contains("not(test)") {
                        self.in_test = true;
                    }
                    self.walk(item, Owner::Top);
                    continue;
                }
                _ => {}
            }
            let taken = std::mem::take(&mut attributes);
            if item.is_named() {
                self.item(item, taken, method_of);
            }
        }
    }

    fn attribute(&mut self, item: Node, attributes: &mut Attributes) {
        if text(item, self.source).starts_with("#[proc_macro") {
            attributes.proc_macro = true;
        }
        let Some(attribute) = item.named_child(0) else {
            return;
        };
        let whole = text(attribute, self.source);
        let name = whole.split(['(', '=', ' ']).next().unwrap_or("");
        let leaf = name.rsplit("::").next().unwrap_or(name);
        if leaf == "test" || leaf.ends_with("_test") || matches!(leaf, "rstest" | "test_case") {
            attributes.test = true;
        }
        if name == "cfg" && whole.contains("test") && !whole.contains("not(test)") {
            attributes.cfg_test = true;
        }
    }

    /// Collect the declared type of every struct field in the file.
    fn struct_fields(&mut self, node: Node) {
        if node.kind() == "struct_item" {
            let name = node.child_by_field_name("name");
            let body = node.child_by_field_name("body");
            if let (Some(name), Some(body)) = (name, body) {
                let name = text(name, self.source).to_string();
                // A generic struct's parameters are not types.
                let outer = self.type_params.clone();
                if let Some(parameters) = node.child_by_field_name("type_parameters") {
                    let mut names = Vec::new();
                    collect(parameters, "type_identifier", &mut names);
                    for parameter in names {
                        self.type_params
                            .insert(text(parameter, self.source).to_string());
                    }
                }
                for field in children(body) {
                    let field_name = field.child_by_field_name("name");
                    let declared = field.child_by_field_name("type");
                    if let (Some(field_name), Some(declared)) = (field_name, declared) {
                        let declared = self.type_name(declared);
                        let key = (name.clone(), text(field_name, self.source).to_string());
                        self.fields.insert(key, declared);
                    }
                }
                self.type_params = outer;
            }
        }
        for child in children(node) {
            self.struct_fields(child);
        }
    }

    /// What is known of the type of the value an expression gives: a type
    /// written down (`self`, a typed binding, a field of either, a string),
    /// or the result of a call.
    fn typing(&self, value: Node) -> Typing {
        let known = |name: Option<String>| name.map_or(Typing::Unknown, Typing::Known);
        match value.kind() {
            "self" => known(self.self_type.clone()),
            "identifier" => self
                .locals
                .get(text(value, self.source))
                .cloned()
                .unwrap_or(Typing::Unknown),
            "string_literal" | "raw_string_literal" => Typing::Known("str".to_string()),
            "struct_expression" => known(
                value
                    .child_by_field_name("name")
                    .and_then(|name| self.type_name(name)),
            ),
            "field_expression" => {
                let (Some(on), Some(field)) = (
                    value.child_by_field_name("value"),
                    value.child_by_field_name("field"),
                ) else {
                    return Typing::Unknown;
                };
                match self.typing(on) {
                    Typing::Known(on) => known(
                        self.fields
                            .get(&(on, text(field, self.source).to_string()))
                            .cloned()
                            .flatten(),
                    ),
                    _ => Typing::Unknown,
                }
            }
            "reference_expression"
            | "parenthesized_expression"
            | "await_expression"
            | "try_expression" => {
                match value.child_by_field_name("value").or(value.named_child(0)) {
                    Some(inner) => self.typing(inner),
                    None => Typing::Unknown,
                }
            }
            "call_expression" => {
                let Some(callee) = value.child_by_field_name("function") else {
                    return Typing::Unknown;
                };
                let first = value
                    .child_by_field_name("arguments")
                    .and_then(|arguments| arguments.named_child(0));
                // `value.name()` where `name` keeps the type.
                if callee.kind() == "field_expression" {
                    let name = callee
                        .child_by_field_name("field")
                        .map(|f| text(f, self.source));
                    let on = callee
                        .child_by_field_name("value")
                        .map(|on| self.typing(on));
                    return match (name, on) {
                        (Some(name), Some(Typing::Result { path, mut through }))
                            if KEEPS_TYPE.contains(&name) =>
                        {
                            through.push(name.to_string());
                            Typing::Result { path, through }
                        }
                        _ => Typing::Unknown,
                    };
                }
                let Some(path) = self.segments(callee) else {
                    return Typing::Unknown;
                };
                let plain: Vec<&str> = path.iter().map(String::as_str).collect();
                if WRAPS.contains(&plain.as_slice()) {
                    return first.map_or(Typing::Unknown, |inner| self.typing(inner));
                }
                match path.first().map(String::as_str) {
                    Some("Self") => match &self.self_type {
                        Some(own) => {
                            let mut full = vec![own.clone()];
                            full.extend(path[1..].iter().cloned());
                            Typing::Result {
                                path: full,
                                through: Vec::new(),
                            }
                        }
                        None => Typing::Unknown,
                    },
                    Some(first) if self.type_params.contains(first) => Typing::Unknown,
                    Some(_) => Typing::Result {
                        path: self.absolute(path),
                        through: Vec::new(),
                    },
                    None => Typing::Unknown,
                }
            }
            _ => Typing::Unknown,
        }
    }

    fn lines(&self, item: Node, attributes: &Attributes) -> (usize, usize) {
        let (start, end) = span(item);
        (attributes.start.unwrap_or(start).min(start), end)
    }

    fn define(&mut self, mut def: Def, attributes: &Attributes) -> Owner {
        def.in_test = self.in_test || attributes.cfg_test || def.kind == DefKind::Test;
        def.callback |= attributes.proc_macro;
        let owner = self.facts.push(def);
        for attribute in &attributes.nodes {
            self.walk(*attribute, owner);
        }
        owner
    }

    fn item(&mut self, item: Node, attributes: Attributes, method_of: Option<&str>) {
        let lines = self.lines(item, &attributes);
        let name = item
            .child_by_field_name("name")
            .map(|name| text(name, self.source).to_string());
        match (item.kind(), name) {
            ("function_item" | "function_signature_item", Some(name)) => {
                let kind = match method_of {
                    Some(_) => DefKind::Method,
                    None if attributes.test => DefKind::Test,
                    None => DefKind::Function,
                };
                // A test is named by its path inside the file, which is how
                // the test harness filters.
                let known_as = if kind == DefKind::Test {
                    let mut path = self.modules.clone();
                    path.push(name.clone());
                    path.join("::")
                } else {
                    name.clone()
                };
                let mut def = Def::new(known_as, kind, lines);
                def.owner = method_of.map(str::to_string);
                def.callback = kind == DefKind::Function && name == "main";
                let owner = self.define(def, &attributes);
                self.function(item, owner);
                self.facts.owner(owner).refs.remove(&name);
            }
            ("struct_item" | "enum_item" | "union_item" | "type_item", Some(name)) => {
                let mut def = Def::new(name.clone(), DefKind::Type, lines);
                def.concrete = item.kind() != "type_item";
                // An alias has the methods of the type it names, where
                // that is one type; of anything otherwise.
                if item.kind() == "type_item" {
                    let mut aliased = None;
                    self.generics(item, |walker| {
                        aliased = item
                            .child_by_field_name("type")
                            .and_then(|target| walker.type_name(target));
                    });
                    def.concrete = aliased.is_some();
                    def.supers = aliased.into_iter().collect();
                }
                let owner = self.define(def, &attributes);
                self.generics(item, |walker| walker.walk(item, owner));
                self.facts.owner(owner).refs.remove(&name);
            }
            ("trait_item", Some(name)) => {
                let mut def = Def::new(name.clone(), DefKind::Type, lines);
                // A trait's methods are its own and its supertraits'.
                def.concrete = true;
                if let Some(bounds) = item.child_by_field_name("bounds") {
                    let mut names = Vec::new();
                    collect(bounds, "type_identifier", &mut names);
                    def.supers = names
                        .iter()
                        .map(|n| text(*n, self.source).to_string())
                        .collect();
                }
                let owner = self.define(def, &attributes);
                let outer = self.self_type.replace(name.clone());
                let outer_trait = std::mem::replace(&mut self.in_trait, true);
                self.generics(item, |walker| {
                    for child in children(item) {
                        if child.kind() == "declaration_list" {
                            walker.items(child, Some(&name));
                        } else {
                            walker.walk(child, owner);
                        }
                    }
                });
                self.in_trait = outer_trait;
                self.self_type = outer;
                self.facts.owner(owner).refs.remove(&name);
            }
            ("impl_item", _) => self.implementation(item, lines, &attributes),
            ("const_item" | "static_item", Some(name)) => {
                let owner = self.define(Def::new(name.clone(), DefKind::Const, lines), &attributes);
                if let Some(declared) = item.child_by_field_name("type") {
                    let mut named = Vec::new();
                    collect_all(declared, "type_identifier", &mut named);
                    let signature: Vec<String> = named
                        .iter()
                        .map(|name| text(*name, self.source).to_string())
                        .collect();
                    self.facts.owner(owner).signature.extend(signature);
                }
                self.walk(item, owner);
                self.facts.owner(owner).refs.remove(&name);
            }
            ("macro_definition", Some(name)) => {
                let owner = self.define(Def::new(name, DefKind::Macro, lines), &attributes);
                self.walk(item, owner);
            }
            ("mod_item", Some(name)) => {
                let Some(body) = item.child_by_field_name("body") else {
                    // `mod name;`: another file.
                    self.facts.submodules.push(super::facts::Submodule {
                        inline: self.modules.clone(),
                        name,
                        test: self.in_test || attributes.cfg_test,
                    });
                    return;
                };
                let outer = self.in_test;
                self.in_test |= attributes.cfg_test;
                self.modules.push(name);
                self.items(body, None);
                self.modules.pop();
                self.in_test = outer;
            }
            ("use_declaration", _) => {
                let before = self.bindings.len();
                let public = children(item)
                    .iter()
                    .any(|c| c.kind() == "visibility_modifier");
                if let Some(argument) = item.child_by_field_name("argument") {
                    self.use_tree(argument, Vec::new(), public);
                }
                let bound = &self.bindings[before..];
                if method_of.is_none() && self.modules.is_empty() {
                    self.facts.imports.push(ImportLine {
                        lines: span(item),
                        names: bound.iter().map(|b| b.local.clone()).collect(),
                        specs: bound.iter().map(|b| b.path.join("::")).collect(),
                        structural: false,
                    });
                }
            }
            ("extern_crate_declaration", Some(name)) => {
                let local = item
                    .child_by_field_name("alias")
                    .map(|alias| text(alias, self.source).to_string())
                    .unwrap_or_else(|| name.clone());
                self.bindings.push(Binding {
                    local,
                    path: vec![name],
                });
            }
            // A macro at item level may define anything. In test code it is
            // taken to define tests, which run by the module they are in.
            ("macro_invocation", _) => {
                let testing = self.in_test || attributes.cfg_test;
                let (label, kind) = if testing {
                    (format!("{}::", self.modules.join("::")), DefKind::Test)
                } else {
                    (format!("macro at line {}", lines.0), DefKind::Const)
                };
                let mut def = Def::new(label, kind, lines);
                def.callback = !testing;
                let owner = self.define(def, &attributes);
                self.walk(item, owner);
            }
            _ => {
                // An item this walk has no name for; its code is the file's.
                let owner = method_of.map_or(Owner::Top, |_| Owner::Top);
                self.walk(item, owner);
            }
        }
    }

    /// `impl Type { … }` or `impl Trait for Type { … }`.
    fn implementation(&mut self, item: Node, lines: (usize, usize), attributes: &Attributes) {
        self.generics(item, |walker| {
            let implemented = item.child_by_field_name("trait");
            let target = item.child_by_field_name("type");
            let type_name = target.and_then(|t| walker.type_name(t));
            let trait_name = implemented.and_then(|t| walker.type_name(t));
            // The block itself stands for the type: a change outside its
            // methods (an associated constant, the bounds) reaches whoever
            // uses the type or the trait.
            let label = type_name
                .clone()
                .or_else(|| trait_name.clone())
                .unwrap_or_else(|| format!("impl at line {}", lines.0));
            let mut def = Def::new(label, DefKind::Type, lines);
            def.concrete = true;
            def.callback = true;
            // The type takes the trait's own methods; a trait that cannot
            // be named adds none that are not written here.
            def.supers = trait_name.clone().into_iter().collect();
            let block = walker.define(def, attributes);
            let outer_in_test = walker.in_test;
            walker.in_test |= attributes.cfg_test;
            let outer = std::mem::replace(&mut walker.self_type, type_name.clone());
            // With no nameable type, `Self` is open, as in a trait.
            let outer_trait = std::mem::replace(&mut walker.in_trait, type_name.is_none());
            let before = walker.facts.defs.len();
            for child in children(item) {
                if child.kind() == "declaration_list" {
                    // With no nameable type, the methods belong to whatever
                    // implements the trait: any type.
                    walker.items(child, Some(type_name.as_deref().unwrap_or(ANY_TYPE)));
                } else {
                    walker.walk(child, block);
                }
            }
            // Users of the type or the trait reach these methods without
            // naming this file.
            let via: Vec<String> = [target, implemented]
                .into_iter()
                .flatten()
                .filter_map(|node| walker.type_path(node))
                .collect();
            let implemented = implemented.and_then(|node| walker.type_path(node));
            walker.facts.owner(block).implements = implemented.clone();
            for def in &mut walker.facts.defs[before..] {
                if def.owner.as_deref() == Some(ANY_TYPE) {
                    def.owner = None;
                }
                def.via.extend(via.iter().cloned());
                if def.kind == DefKind::Method {
                    def.implements = implemented.clone();
                }
            }
            walker.facts.owner(block).via.extend(via);
            walker.self_type = outer;
            walker.in_trait = outer_trait;
            walker.in_test = outer_in_test;
        });
    }

    /// Run `body` with the item's type parameters in scope.
    fn generics(&mut self, item: Node, body: impl FnOnce(&mut Self)) {
        let outer = self.type_params.clone();
        if let Some(parameters) = item.child_by_field_name("type_parameters") {
            for parameter in children(parameters) {
                let name = match parameter.kind() {
                    "type_identifier" => Some(parameter),
                    _ => parameter.child_by_field_name("name"),
                };
                if let Some(name) = name.filter(|n| n.kind() == "type_identifier") {
                    self.type_params.insert(text(name, self.source).to_string());
                }
            }
        }
        body(self);
        self.type_params = outer;
    }

    /// The type names in what a function returns, or `None` when the
    /// declaration does not fix them.
    fn returns(&self, item: Node) -> Option<Vec<String>> {
        let Some(declared) = item.child_by_field_name("return_type") else {
            return Some(Vec::new());
        };
        let mut open = Vec::new();
        collect_all(declared, "abstract_type", &mut open);
        collect_all(declared, "dynamic_type", &mut open);
        collect_all(declared, "scoped_type_identifier", &mut open);
        if !open.is_empty() {
            return None;
        }
        let mut names = Vec::new();
        collect_all(declared, "type_identifier", &mut names);
        names
            .into_iter()
            .map(|name| match text(name, self.source) {
                "Self" => self.self_type.clone(),
                name if self.type_params.contains(name) => None,
                name => Some(name.to_string()),
            })
            .collect()
    }

    fn function(&mut self, item: Node, owner: Owner) {
        let outer = std::mem::take(&mut self.locals);
        self.generics(item, |walker| {
            // Type parameters of the function or of what encloses it, or a
            // trait object anywhere in it.
            let mut open = Vec::new();
            collect(item, "abstract_type", &mut open);
            collect(item, "dynamic_type", &mut open);
            let generic = !walker.type_params.is_empty() || !open.is_empty() || walker.in_trait;
            walker.facts.owner(owner).generic = generic;
            // Everything but the body: parameters, return type, bounds.
            let mut named = Vec::new();
            for part in children(item) {
                if part.kind() != "block" {
                    collect_all(part, "type_identifier", &mut named);
                }
            }
            let signature: Vec<String> = named
                .iter()
                .map(|name| text(*name, walker.source).to_string())
                .collect();
            walker.facts.owner(owner).signature.extend(signature);
            walker.facts.owner(owner).returns = walker.returns(item);
            walker.declared(item);
            walker.walk(item, owner);
        });
        self.locals = outer;
    }

    /// The type a type expression names, where a method call on a value of
    /// it goes to that type: `T`, `&T`, `Box<T>`, `dyn Trait`.
    fn type_name(&self, node: Node) -> Option<String> {
        match node.kind() {
            "type_identifier" => {
                let name = text(node, self.source);
                match name {
                    "Self" => self.self_type.clone(),
                    _ if self.type_params.contains(name) => None,
                    _ => Some(name.to_string()),
                }
            }
            "primitive_type" => Some(text(node, self.source).to_string()),
            // `module::Type`; not `Self::Item` or `T::Item`, which stand
            // for whatever the implementation chose.
            "scoped_type_identifier" => {
                let path = self.segments(node.child_by_field_name("path")?)?;
                let first = path.first()?;
                if first == "Self" || self.type_params.contains(first) {
                    return None;
                }
                node.child_by_field_name("name")
                    .map(|name| text(name, self.source).to_string())
            }
            "reference_type" | "pointer_type" => self.type_name(node.child_by_field_name("type")?),
            "dynamic_type" | "abstract_type" => self.type_name(node.child_by_field_name("trait")?),
            "generic_type" => {
                let base = node.child_by_field_name("type")?;
                let name = self.type_name(base)?;
                if !POINTERS.contains(&name.as_str()) {
                    return Some(name);
                }
                let arguments = node.child_by_field_name("type_arguments")?;
                self.type_name(arguments.named_child(0)?)
            }
            _ => None,
        }
    }

    /// The path a type expression names, for resolving to the file that
    /// declares it.
    fn type_path(&mut self, node: Node) -> Option<String> {
        let node = match node.kind() {
            "generic_type" => node.child_by_field_name("type")?,
            "reference_type" | "pointer_type" => node.child_by_field_name("type")?,
            _ => node,
        };
        let segments = self.segments(node)?;
        if segments.len() == 1 && self.type_params.contains(&segments[0]) {
            return None;
        }
        Some(self.absolute(segments).join("::"))
    }

    /// Note that the function binds `name`, with its type when written. Two
    /// bindings that disagree leave the type unknown.
    fn declare(&mut self, name: &str, declared: Typing) {
        match self.locals.get_mut(name) {
            // The same call, reached through different type-keeping
            // methods (`let x = x.clone();`), is still that call.
            Some(Typing::Result { path, through }) => match declared {
                Typing::Result {
                    path: again,
                    through: more,
                } if again == *path => {
                    for name in more {
                        if !through.contains(&name) {
                            through.push(name);
                        }
                    }
                }
                _ => {
                    self.locals.insert(name.to_string(), Typing::Unknown);
                }
            },
            Some(known) if *known != declared => *known = Typing::Unknown,
            Some(_) => {}
            None => {
                self.locals.insert(name.to_string(), declared);
            }
        }
    }

    /// Collect every name a function binds, before its body is read.
    fn declared(&mut self, node: Node) {
        if node.kind() == "closure_parameters" {
            self.bound(node);
        }
        if let Some(pattern) = node.child_by_field_name("pattern") {
            let typed = matches!(node.kind(), "parameter" | "let_declaration");
            let plain = match pattern.kind() {
                "identifier" => Some(pattern),
                "mut_pattern" => pattern.named_child(0).filter(|p| p.kind() == "identifier"),
                _ => None,
            };
            match plain.filter(|_| typed) {
                Some(name) => {
                    // The written type decides; failing that, what the
                    // value is.
                    let written = node
                        .child_by_field_name("type")
                        .map(|t| self.type_name(t).map_or(Typing::Unknown, Typing::Known));
                    let declared = written.unwrap_or_else(|| {
                        node.child_by_field_name("value")
                            .map_or(Typing::Unknown, |value| self.typing(value))
                    });
                    self.declare(text(name, self.source), declared);
                }
                None => self.bound(pattern),
            }
        }
        for child in children(node) {
            self.declared(child);
        }
    }

    /// Every identifier under a pattern is a name of unknown type.
    fn bound(&mut self, node: Node) {
        if node.kind() == "identifier" {
            self.declare(text(node, self.source), Typing::Unknown);
        }
        for child in children(node) {
            self.bound(child);
        }
    }

    /// The segments of a path, or `None` when it is not a plain one.
    fn segments(&self, node: Node) -> Option<Vec<String>> {
        match node.kind() {
            "identifier" | "type_identifier" | "crate" | "super" | "self" | "metavariable" => {
                Some(vec![text(node, self.source).to_string()])
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let name = text(node.child_by_field_name("name")?, self.source).to_string();
                let mut segments = match node.child_by_field_name("path") {
                    Some(path) => self.segments(path)?,
                    // `::name`: a crate.
                    None => Vec::new(),
                };
                segments.push(name);
                Some(segments)
            }
            "generic_type" | "generic_type_with_turbofish" => {
                self.segments(node.child_by_field_name("type")?)
            }
            _ => None,
        }
    }

    /// Make a path as absolute as the file allows: `super` out of an inline
    /// module, and `self` inside one, stay in this file.
    fn absolute(&self, mut segments: Vec<String>) -> Vec<String> {
        let depth = self.modules.len();
        let supers = segments.iter().take_while(|s| *s == "super").count();
        if supers > 0 && supers <= depth {
            segments.splice(..supers, ["self".to_string()]);
        } else if supers > depth {
            segments.drain(..depth);
        }
        segments
    }

    fn path(&mut self, owner: Owner, segments: Vec<String>) {
        let segments = self.absolute(segments);
        self.paths.push((owner, segments));
    }

    fn member(&mut self, owner: Owner, on: Option<String>, name: &str) {
        let def = self.facts.owner(owner);
        match on {
            Some(on) => def.typed.insert((on, name.to_string())),
            None => def.members.insert(name.to_string()),
        };
    }

    /// A path with more than one segment, in an expression or a type.
    fn scoped(&mut self, node: Node, owner: Owner) {
        // `<T as Trait>::name`.
        let qualified = node
            .child_by_field_name("path")
            .filter(|path| path.kind() == "bracketed_type");
        if let Some(qualified) = qualified {
            let name = node.child_by_field_name("name");
            let implemented = qualified
                .named_child(0)
                .and_then(|inner| inner.child_by_field_name("alias"))
                .and_then(|alias| self.type_name(alias));
            if let Some(name) = name {
                let name = text(name, self.source).to_string();
                self.member(owner, implemented, &name);
            }
            self.walk(qualified, owner);
            return;
        }
        let Some(segments) = self.segments(node) else {
            for child in children(node) {
                self.walk(child, owner);
            }
            return;
        };
        self.arguments(node, owner);
        match segments.first().map(String::as_str) {
            Some("Self") => {
                if let Some(name) = segments.get(1) {
                    let on = self.self_type.clone();
                    self.member(owner, on, name);
                }
            }
            // `T::name()` on a type parameter: any type's.
            Some(first) if self.type_params.contains(first) => {
                if let Some(name) = segments.get(1) {
                    self.member(owner, None, name);
                }
            }
            _ => self.path(owner, segments),
        }
    }

    /// The type arguments inside a path (`Vec::<Foo>::new`).
    fn arguments(&mut self, node: Node, owner: Owner) {
        for child in children(node) {
            match child.kind() {
                "type_arguments" => self.walk(child, owner),
                "scoped_identifier"
                | "scoped_type_identifier"
                | "generic_type"
                | "generic_type_with_turbofish" => self.arguments(child, owner),
                _ => {}
            }
        }
    }

    fn walk(&mut self, node: Node, owner: Owner) {
        match node.kind() {
            "line_comment" | "block_comment" => {
                if !is_doc(text(node, self.source)) {
                    self.inert.push((node.start_byte(), node.end_byte()));
                }
                return;
            }
            "identifier" | "type_identifier" => {
                let name = text(node, self.source);
                if name != "Self" && !self.type_params.contains(name) {
                    if BINARY_MARKERS.contains(&name) {
                        self.facts.owner(owner).reflects = Some("runs the package's binary");
                    }
                    self.facts.owner(owner).refs.insert(name.to_string());
                }
                return;
            }
            "field_identifier" | "shorthand_field_identifier" => return,
            "string_content" => {
                let value = text(node, self.source).to_string();
                if value.contains("CARGO_BIN_") {
                    self.facts.owner(owner).reflects = Some("runs the package's binary");
                }
                self.facts.owner(owner).string(&value);
                return;
            }
            "scoped_identifier" | "scoped_type_identifier" => return self.scoped(node, owner),
            // `value.name(…)` calls a method; `value.name` alone reads a
            // field, which is no call (a method is only passed around as
            // `Type::name`).
            "call_expression" => {
                let mut callee = node.child_by_field_name("function");
                if let Some(generic) = callee.filter(|c| c.kind() == "generic_function") {
                    callee = generic.child_by_field_name("function");
                }
                if let Some(callee) = callee.filter(|c| c.kind() == "field_expression") {
                    let value = callee.child_by_field_name("value");
                    if let Some(field) = callee.child_by_field_name("field") {
                        let field = text(field, self.source).to_string();
                        match value.map_or(Typing::Unknown, |value| self.typing(value)) {
                            Typing::Known(on) => self.member(owner, Some(on), &field),
                            Typing::Unknown => self.member(owner, None, &field),
                            Typing::Result { path, through } => {
                                self.results.push((owner, path, through, field));
                            }
                        }
                    }
                }
            }
            "macro_invocation" => {
                // A macro is named bare wherever it is in scope.
                let name = node
                    .child_by_field_name("macro")
                    .and_then(|name| self.segments(name))
                    .and_then(|segments| segments.last().cloned());
                // `name!`, apart from any method of the same name.
                if let Some(name) = name {
                    self.facts.owner(owner).members.insert(format!("{name}!"));
                }
            }
            "token_tree" => return self.tokens(node, owner),
            // Items inside a function body.
            "use_declaration" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    self.use_tree(argument, Vec::new(), false);
                }
                return;
            }
            _ => {}
        }
        for child in children(node) {
            self.walk(child, owner);
        }
    }

    /// The arguments of a macro: tokens, not syntax. Paths and member
    /// accesses are read off the punctuation between names.
    fn tokens(&mut self, tree: Node, owner: Owner) {
        let mut path: Vec<String> = Vec::new();
        let mut last = "";
        let mut receiver: Option<String> = None;
        let flush = |walker: &mut Self, path: &mut Vec<String>| {
            match path.len() {
                0 => {}
                1 => {
                    let name = path[0].clone();
                    if !matches!(name.as_str(), "self" | "Self" | "crate" | "super")
                        && !walker.type_params.contains(&name)
                    {
                        if BINARY_MARKERS.contains(&name.as_str()) {
                            walker.facts.owner(owner).reflects = Some("runs the package's binary");
                        }
                        walker.facts.owner(owner).refs.insert(name);
                    }
                }
                _ => match path[0].as_str() {
                    "Self" => {
                        let on = walker.self_type.clone();
                        walker.member(owner, on, &path[1].clone());
                    }
                    first if walker.type_params.contains(first) => {
                        walker.member(owner, None, &path[1].clone());
                    }
                    _ => walker.path(owner, path.clone()),
                },
            }
            path.clear();
        };
        for token in children(tree) {
            let kind = token.kind();
            match kind {
                "identifier" | "crate" | "super" | "self" | "metavariable" | "primitive_type" => {
                    let name = text(token, self.source).to_string();
                    if last == "::" && !path.is_empty() {
                        path.push(name);
                    } else if last == "." {
                        // `value.name(…)`: a method call. Without the
                        // arguments it reads a field.
                        let on = receiver.take().and_then(|value| match value.as_str() {
                            "self" => self.self_type.clone(),
                            value => match self.locals.get(value) {
                                Some(Typing::Known(name)) => Some(name.clone()),
                                _ => None,
                            },
                        });
                        let called = token.next_sibling().is_some_and(|next| {
                            let next = text(next, self.source);
                            next.starts_with('(') || next == "::"
                        });
                        if called {
                            self.member(owner, on, &name);
                        }
                    } else {
                        flush(self, &mut path);
                        path.push(name);
                    }
                }
                "::" => {}
                "." => {
                    receiver = (path.len() == 1).then(|| path[0].clone());
                    flush(self, &mut path);
                }
                "token_tree" => {
                    flush(self, &mut path);
                    self.tokens(token, owner);
                }
                "string_literal" | "raw_string_literal" => {
                    flush(self, &mut path);
                    self.walk(token, owner);
                }
                _ => flush(self, &mut path),
            }
            last = kind;
        }
        flush(self, &mut path);
    }

    /// One `use` tree under `prefix`.
    fn use_tree(&mut self, node: Node, prefix: Vec<String>, public: bool) {
        let join = |walker: &Self, prefix: &[String], node: Node| -> Option<Vec<String>> {
            let mut path = prefix.to_vec();
            path.extend(walker.segments(node)?);
            Some(path)
        };
        match node.kind() {
            "use_list" => {
                for child in children(node) {
                    if child.is_named() {
                        self.use_tree(child, prefix.clone(), public);
                    }
                }
            }
            "scoped_use_list" => {
                let path = match node.child_by_field_name("path") {
                    Some(path) => join(self, &prefix, path),
                    None => Some(prefix),
                };
                if let (Some(path), Some(list)) = (path, node.child_by_field_name("list")) {
                    self.use_tree(list, path, public);
                }
            }
            "use_wildcard" => {
                let path = match node.named_child(0) {
                    Some(inner) => join(self, &prefix, inner),
                    None => Some(prefix),
                };
                if let Some(path) = path {
                    let spec = self.absolute(path).join("::");
                    if public {
                        self.facts.reexports.push(spec.clone());
                    }
                    self.facts.uses_all.push(spec);
                }
            }
            "use_as_clause" => {
                let path = node
                    .child_by_field_name("path")
                    .and_then(|path| join(self, &prefix, path));
                let alias = node
                    .child_by_field_name("alias")
                    .map(|alias| text(alias, self.source).to_string());
                if let (Some(path), Some(alias)) = (path, alias) {
                    self.bind(alias, path, public);
                }
            }
            _ => {
                let Some(mut path) = join(self, &prefix, node) else {
                    return;
                };
                // `use a::b::{self}` binds `b`.
                if path.last().is_some_and(|last| last == "self") && path.len() > 1 {
                    path.pop();
                }
                if let Some(local) = path.last().cloned() {
                    self.bind(local, path, public);
                }
            }
        }
    }

    fn bind(&mut self, local: String, path: Vec<String>, public: bool) {
        let path = self.absolute(path);
        if public {
            self.facts.reexports.push(path.join("::"));
        }
        self.bindings.push(Binding { local, path });
    }

    /// Replace the first segment of each path by what `use` bound it to,
    /// and record every path against its definition.
    fn apply_bindings(&mut self) {
        let bindings = std::mem::take(&mut self.bindings);
        let record = |facts: &mut FileFacts, owner: Owner, path: &[String]| {
            let rooted = matches!(path[0].as_str(), "crate" | "super");
            let def = facts.owner(owner);
            if !rooted {
                // It may name something in this file.
                def.refs.extend(path.iter().cloned());
            }
            def.calls.insert((path.join("::"), String::new()));
        };
        for (owner, path) in std::mem::take(&mut self.paths) {
            let bound: Vec<&Binding> = bindings.iter().filter(|b| b.local == path[0]).collect();
            if bound.is_empty() {
                record(&mut self.facts, owner, &path);
            }
            for binding in bound {
                let mut full = binding.path.clone();
                full.extend(path[1..].iter().cloned());
                record(&mut self.facts, owner, &full);
            }
        }
        for (owner, path, through, member) in std::mem::take(&mut self.results) {
            let bound: Vec<&Binding> = bindings.iter().filter(|b| b.local == path[0]).collect();
            let mut paths: Vec<Vec<String>> = bound
                .iter()
                .map(|binding| {
                    let mut full = binding.path.clone();
                    full.extend(path[1..].iter().cloned());
                    full
                })
                .collect();
            if paths.is_empty() {
                paths.push(path);
            }
            for path in paths {
                record(&mut self.facts, owner, &path);
                let result = (path.join("::"), through.clone(), member.clone());
                self.facts.owner(owner).results.push(result);
            }
        }
        // The paths of an `impl`'s type and trait are written like any
        // other: through what `use` bound.
        let resolved = |path: &str| -> String {
            let (first, rest) = path.split_once("::").unwrap_or((path, ""));
            match bindings.iter().find(|b| b.local == first) {
                Some(binding) if rest.is_empty() => binding.path.join("::"),
                Some(binding) => format!("{}::{rest}", binding.path.join("::")),
                None => path.to_string(),
            }
        };
        for def in &mut self.facts.defs {
            for via in &mut def.via {
                *via = resolved(via);
            }
            if let Some(implemented) = &mut def.implements {
                *implemented = resolved(implemented);
            }
        }
        // A type imported under another name (`use a::Foo as Bar`) is
        // still `Foo` to everything that matches types by name.
        let renamed: HashMap<&str, &str> = bindings
            .iter()
            .filter_map(|b| Some((b.local.as_str(), b.path.last()?.as_str())))
            .filter(|(local, real)| local != real)
            .collect();
        if !renamed.is_empty() {
            let real = |name: &mut String| {
                if let Some(real) = renamed.get(name.as_str()) {
                    *name = real.to_string();
                }
            };
            for def in self.facts.defs.iter_mut().chain([&mut self.facts.top]) {
                def.owner.iter_mut().for_each(real);
                def.supers.iter_mut().for_each(real);
                def.returns.iter_mut().flatten().for_each(real);
                if def.kind == DefKind::Type && def.callback {
                    real(&mut def.name);
                }
                def.signature = std::mem::take(&mut def.signature)
                    .into_iter()
                    .map(|mut name| {
                        real(&mut name);
                        name
                    })
                    .collect();
                def.typed = std::mem::take(&mut def.typed)
                    .into_iter()
                    .map(|(mut on, member)| {
                        real(&mut on);
                        (on, member)
                    })
                    .collect();
            }
        }
        let mut owners: Vec<Owner> = (0..self.facts.defs.len()).map(Owner::Def).collect();
        owners.push(Owner::Top);
        let mut named = vec![false; bindings.len()];
        for owner in owners {
            for (index, binding) in bindings.iter().enumerate() {
                let def = self.facts.owner(owner);
                let on_type = def.typed.iter().any(|(on, _)| *on == binding.local);
                if def.refs.contains(&binding.local) || on_type {
                    named[index] = true;
                    record(&mut self.facts, owner, &binding.path);
                }
            }
        }
        // An import nothing names is there for what it brings into scope:
        // a trait's methods.
        for (binding, _) in bindings.iter().zip(named).filter(|(_, named)| !named) {
            self.facts.brings.push(binding.path.join("::"));
        }
    }
}

/// Every descendant of `node` of `kind`, `node` itself included.
fn collect_all<'t>(node: Node<'t>, kind: &str, out: &mut Vec<Node<'t>>) {
    if node.kind() == kind {
        out.push(node);
    }
    for child in children(node) {
        collect_all(child, kind, out);
    }
}

/// Every descendant of `node` of `kind`, not looking inside one.
fn collect<'t>(node: Node<'t>, kind: &str, out: &mut Vec<Node<'t>>) {
    for child in children(node) {
        if child.kind() == kind {
            out.push(child);
        } else {
            collect(child, kind, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::related::facts::LineClass;

    fn def<'f>(facts: &'f FileFacts, name: &str) -> &'f Def {
        facts
            .defs
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no definition named {name}"))
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn extracts_items_methods_and_tests_beside_the_code() {
        let source = r#"use crate::pricing::{self, Rate as Tariff};
use std::fmt;
pub use crate::model::*;

/// Holds items.
#[derive(Debug)]
pub struct Cart {
    pub items: Vec<u32>,
    rate: Tariff,
}

pub type Shared = std::sync::Arc<Cart>;

impl Cart {
    pub fn total(&self) -> u32 {
        pricing::sum(&self.items) + self.rate.apply(1)
    }
}

impl fmt::Display for Cart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.total())
    }
}

pub trait Priced: Sized {
    fn price(&self) -> u32;
    fn doubled(&self) -> u32 {
        self.price() * 2
    }
}

macro_rules! cart {
    () => {
        Cart { items: Vec::new() }
    };
}

mod child;
#[cfg(test)]
mod support;

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Cart {
        cart!()
    }

    #[test]
    fn totals() {
        assert_eq!(fixture().total(), 0);
    }

    #[tokio::test]
    async fn waits() {}
}
"#;
        let facts = extract(Path::new("src/cart.rs"), source);
        assert_eq!(facts.reexports, ["crate::model"]);
        // `use super::*` in the test module is this file itself.
        assert_eq!(facts.uses_all, ["crate::model", "self"]);

        let cart = def(&facts, "Cart");
        assert_eq!(cart.kind, DefKind::Type);
        assert!(cart.concrete && !cart.in_test);
        // Documentation and attributes belong to the item.
        assert_eq!(cart.lines.0, 5);
        assert_eq!(facts.class_of(5), LineClass::Def(0));
        // A derive is code the item uses.
        assert!(cart.refs.contains("Debug"));

        // An alias has the methods of what it names, through a pointer.
        let shared = def(&facts, "Shared");
        assert!(shared.concrete);
        assert_eq!(shared.supers, ["Cart"]);

        let total = def(&facts, "total");
        assert_eq!(total.kind, DefKind::Method);
        assert_eq!(total.owner.as_deref(), Some("Cart"));
        assert!(total.calls.contains(&pair("crate::pricing::sum", "")));
        // `self.rate` is a `Tariff`, which the import calls `Rate`.
        assert!(total.typed.contains(&pair("Rate", "apply")));
        // Reading a field is not a call.
        assert!(!total.typed.iter().any(|(_, name)| name == "items"));
        assert_eq!(total.returns.as_deref(), Some(&[][..]));

        // A method of a trait from elsewhere, called through `write!`.
        let fmt = def(&facts, "fmt");
        assert_eq!(fmt.implements.as_deref(), Some("std::fmt::Display"));
        assert!(fmt.typed.contains(&pair("Cart", "total")));
        assert!(fmt.members.contains("write!"));

        let priced = def(&facts, "Priced");
        assert_eq!(priced.supers, ["Sized"]);
        let doubled = def(&facts, "doubled");
        assert_eq!(doubled.owner.as_deref(), Some("Priced"));
        assert!(doubled.generic);
        assert!(doubled.typed.contains(&pair("Priced", "price")));

        assert_eq!(def(&facts, "cart").kind, DefKind::Macro);

        let submodules: Vec<(&str, bool)> = facts
            .submodules
            .iter()
            .map(|s| (s.name.as_str(), s.test))
            .collect();
        assert_eq!(submodules, [("child", false), ("support", true)]);

        // Tests are named by their path in the file; what sits beside
        // them in the test module is test code too.
        let totals = def(&facts, "tests::totals");
        assert_eq!(totals.kind, DefKind::Test);
        assert!(totals.in_test);
        assert!(totals.members.contains("assert_eq!"));
        assert_eq!(def(&facts, "tests::waits").kind, DefKind::Test);
        let fixture = def(&facts, "fixture");
        assert!(fixture.in_test && fixture.kind == DefKind::Function);
        assert!(fixture.members.contains("cart!"));
        assert_eq!(fixture.returns.as_deref(), Some(&["Cart".to_string()][..]));
    }

    #[test]
    fn types_a_value_by_the_call_that_made_it() {
        let source = r#"use crate::store::Store;
use std::sync::Arc;

pub fn open<T: Clone>(seed: T, raw: &str) -> usize {
    let store = Arc::new(Store::new(raw));
    let store = store.clone();
    let other = build();
    let unknown = raw.parse::<u32>().map(|n| n + 1);
    store.load();
    other.save();
    unknown.count();
    seed.clone();
    T::make();
    Self::helper();
    0
}
"#;
        let facts = extract(Path::new("src/open.rs"), source);
        let open = def(&facts, "open");
        // The second binding of `store` is the first, cloned.
        assert!(open.results.contains(&(
            "crate::store::Store::new".to_string(),
            vec!["clone".to_string()],
            "load".to_string()
        )));
        assert!(open
            .results
            .contains(&("build".to_string(), Vec::new(), "save".to_string())));
        // `raw` is a `str`; what `map` gives back is anyone's guess.
        assert!(open.typed.contains(&pair("str", "parse")));
        assert!(open.members.contains("count"));
        // A value of a type parameter, and the parameter itself.
        assert!(open.members.contains("clone") && open.members.contains("make"));
        assert!(open.generic);
        assert!(open.signature.contains("Clone"));
    }

    #[test]
    fn a_test_that_runs_the_binary_says_so() {
        let source = "#[test]\nfn runs() {\n    let exe = env!(\"CARGO_BIN_EXE_shop\");\n    std::process::Command::new(exe);\n}\n";
        let facts = extract(Path::new("tests/cli.rs"), source);
        assert_eq!(
            def(&facts, "runs").reflects,
            Some("runs the package's binary")
        );
    }
}
