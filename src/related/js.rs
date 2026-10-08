//! TypeScript and JavaScript facts: top-level declarations, class members,
//! import bindings and test blocks.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner, ANY_TYPE,
};
use std::collections::HashSet;
use std::path::Path;
use tree_sitter::Node;

const TEST_CALLS: &[&str] = &["it", "test", "bench", "specify"];

/// A name bound by an import: `local` stands for `imported` from `spec`.
struct Binding {
    local: String,
    spec: String,
    /// `None` for a namespace import.
    imported: Option<String>,
}

struct Walker<'a> {
    source: &'a str,
    facts: FileFacts,
    bindings: Vec<Binding>,
    inert: Vec<(usize, usize)>,
    has_tests: bool,
    /// The class whose instance `this` is, where that is certain.
    this_type: Option<String>,
    /// Names the file binds other than by importing them.
    bound: HashSet<String>,
    /// `name.member` accesses waiting to learn what `name` is.
    named_members: Vec<(Owner, String, String)>,
}

/// Objects every JavaScript runtime provides. A member of one is not a
/// method of anything a workspace declares.
const GLOBALS: &[&str] = &[
    "Array",
    "BigInt",
    "Boolean",
    "Buffer",
    "Date",
    "Error",
    "Intl",
    "JSON",
    "Map",
    "Math",
    "Number",
    "Object",
    "Promise",
    "Reflect",
    "RegExp",
    "Set",
    "String",
    "Symbol",
    "URL",
    "URLSearchParams",
    "WeakMap",
    "WeakSet",
    "console",
    "crypto",
    "document",
    "globalThis",
    "localStorage",
    "navigator",
    "performance",
    "process",
    "sessionStorage",
    "window",
];

pub fn extract(path: &Path, source: &str) -> FileFacts {
    let mut facts = FileFacts::new(Family::Js);
    facts.members = true;
    facts.loads_by_import = true;
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // The TSX grammar reads JavaScript and JSX; only `.ts` needs the plain
    // one, where `<T>value` is a cast rather than an element.
    let language = if matches!(extension, "ts" | "mts" | "cts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    } else {
        tree_sitter_typescript::LANGUAGE_TSX
    };
    let Some(tree) = parse(language.into(), source) else {
        facts.parse_error = true;
        return facts;
    };
    facts.parse_error = tree.root_node().has_error();
    let mut walker = Walker {
        source,
        facts,
        bindings: Vec::new(),
        inert: Vec::new(),
        has_tests: false,
        this_type: None,
        bound: HashSet::new(),
        named_members: Vec::new(),
    };
    walker.bind(tree.root_node());
    for child in children(tree.root_node()) {
        walker.statement(child, child);
    }
    walker.apply_bindings();
    let mut facts = walker.facts;
    facts.is_test_file = is_test_path(path) || (walker.has_tests && in_test_dir(path));
    facts.classify_lines(source, &walker.inert);
    facts
}

fn is_test_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    [".test.", ".spec.", "_test.", "_spec."]
        .iter()
        .any(|marker| name.contains(marker))
}

fn in_test_dir(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("__tests__" | "test" | "tests")))
}

/// The identifier a call ultimately invokes: `test` in `test.each(x)(…)`.
fn callee_root<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" => Some(text(node, source)),
        "member_expression" => callee_root(node.child_by_field_name("object")?, source),
        "call_expression" => callee_root(node.child_by_field_name("function")?, source),
        _ => None,
    }
}

fn string_value<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    (node.kind() == "string")
        .then(|| text(node, source).trim_matches(|c| c == '"' || c == '\'' || c == '`'))
}

/// The specifier loaded by `require("x")` or `import("x")`, through `await`.
fn loaded_spec<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    match node.kind() {
        "await_expression" | "parenthesized_expression" => {
            loaded_spec(node.named_child(0)?, source)
        }
        "call_expression" => {
            let function = node.child_by_field_name("function")?;
            let loads = function.kind() == "import" || text(function, source) == "require";
            let arguments = node.child_by_field_name("arguments")?;
            loads
                .then(|| string_value(arguments.named_child(0)?, source))
                .flatten()
        }
        _ => None,
    }
}

impl Walker<'_> {
    /// A top-level statement. `whole` is the statement including `export`.
    fn statement(&mut self, node: Node, whole: Node) {
        match node.kind() {
            "comment" => self.inert.push((node.start_byte(), node.end_byte())),
            "import_statement" => {
                let before = self.bindings.len();
                self.import(node);
                self.import_line(node, before);
            }
            "export_statement" => self.export(node),
            "ambient_declaration" => {
                for child in children(node) {
                    if child.is_named() {
                        self.statement(child, whole);
                    }
                }
            }
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                self.named(node, whole, DefKind::Function);
            }
            "class_declaration" | "abstract_class_declaration" => self.class(node, whole),
            "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "internal_module"
            | "module" => {
                self.named(node, whole, DefKind::Type);
            }
            "lexical_declaration" | "variable_declaration" => {
                let declarators: Vec<Node> = children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "variable_declarator")
                    .collect();
                let single = declarators.len() == 1;
                for declarator in declarators {
                    self.declarator(declarator, if single { whole } else { declarator });
                }
            }
            "expression_statement" if self.commonjs_export(node) => {}
            _ => self.walk(node, Owner::Top),
        }
    }

    /// `module.exports = …` and `exports.x = …`. A list of names exports
    /// definitions that importers find themselves; anything else is the
    /// module's default export.
    fn commonjs_export(&mut self, node: Node) -> bool {
        let Some(assignment) = node
            .named_child(0)
            .filter(|n| n.kind() == "assignment_expression")
        else {
            return false;
        };
        let (Some(left), Some(right)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("right"),
        ) else {
            return false;
        };
        let target = text(left, self.source);
        if target != "module.exports" && !target.starts_with("exports.") {
            return false;
        }
        match right.kind() {
            "identifier" => {}
            "object"
                if children(right).iter().all(|c| {
                    !c.is_named() || matches!(c.kind(), "shorthand_property_identifier" | "comment")
                }) => {}
            _ => {
                let name = target.strip_prefix("exports.").unwrap_or("default");
                let owner = self.facts.push(Def::new(name, DefKind::Const, span(node)));
                self.walk(right, owner);
            }
        }
        true
    }

    fn named(&mut self, node: Node, whole: Node, kind: DefKind) -> Option<Owner> {
        let name = node
            .child_by_field_name("name")
            .map(|n| text(n, self.source).to_string())
            .unwrap_or_else(|| "default".to_string());
        let owner = self.facts.push(Def::new(name.clone(), kind, span(whole)));
        self.walk(node, owner);
        self.facts.owner(owner).refs.remove(&name);
        Some(owner)
    }

    fn class(&mut self, node: Node, whole: Node) {
        let name = node
            .child_by_field_name("name")
            .map(|n| text(n, self.source).to_string())
            .unwrap_or_else(|| "default".to_string());
        let mut def = Def::new(name.clone(), DefKind::Type, span(whole));
        def.concrete = true;
        def.supers = self.extended(node);
        let class = self.facts.push(def);
        let outer = self.this_type.replace(name.clone());
        for child in children(node) {
            if child.kind() != "class_body" {
                self.walk(child, class);
                continue;
            }
            for member in children(child) {
                let member_name = member
                    .child_by_field_name("name")
                    .map(|n| text(n, self.source).to_string());
                let is_method = member.kind() == "method_definition"
                    || (member.kind() == "public_field_definition"
                        && member.child_by_field_name("value").is_some_and(|v| {
                            matches!(v.kind(), "arrow_function" | "function_expression")
                        }));
                match member_name {
                    // Constructing the class is a use of the class itself.
                    Some(member_name) if is_method && member_name != "constructor" => {
                        let mut method =
                            Def::new(member_name.clone(), DefKind::Method, span(member));
                        method.owner = Some(name.clone());
                        let owner = self.facts.push(method);
                        // The member's own `this` is the instance; walking
                        // its parts keeps that.
                        for part in children(member) {
                            self.walk(part, owner);
                        }
                        self.facts.owner(owner).refs.remove(&member_name);
                    }
                    _ => {
                        for part in children(member) {
                            self.walk(part, class);
                        }
                    }
                }
            }
        }
        self.this_type = outer;
        self.facts.owner(class).refs.remove(&name);
    }

    /// The classes a class extends. One that is not a plain name (a mixin
    /// call, `ns.Base`) cannot be followed.
    fn extended(&self, class: Node) -> Vec<String> {
        let mut clauses = Vec::new();
        for heritage in children(class) {
            if heritage.kind() != "class_heritage" {
                continue;
            }
            let extends: Vec<Node> = children(heritage)
                .into_iter()
                .filter(|c| c.kind() == "extends_clause")
                .collect();
            if extends.is_empty() {
                // JavaScript: the heritage is the expression itself.
                clauses.extend(children(heritage).into_iter().filter(|c| c.is_named()));
            }
            for clause in extends {
                clauses.extend(clause.child_by_field_name("value"));
            }
        }
        clauses
            .into_iter()
            .filter(|c| c.kind() != "implements_clause")
            .map(|value| match value.kind() {
                "identifier" => text(value, self.source).to_string(),
                _ => ANY_TYPE.to_string(),
            })
            .collect()
    }

    /// Collect every name the file binds by declaring it, so that a name
    /// bound only by an import, or not at all, can be told apart.
    fn bind(&mut self, node: Node) {
        let field = |name: &str| node.child_by_field_name(name);
        let pattern = match node.kind() {
            "import_statement" => return,
            "variable_declarator" => {
                // `const x = require("y")` is an import.
                let loads = field("value").is_some_and(|v| loaded_spec(v, self.source).is_some());
                field("name").filter(|_| !loads)
            }
            "required_parameter" | "optional_parameter" => field("pattern"),
            "arrow_function" => field("parameter"),
            "catch_clause" => field("parameter"),
            "for_in_statement" => field("left"),
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "enum_declaration"
            | "internal_module" => field("name"),
            // JavaScript parameters are bare patterns.
            "formal_parameters" => Some(node),
            _ => None,
        };
        if let Some(pattern) = pattern {
            self.bind_names(pattern);
        }
        for child in children(node) {
            self.bind(child);
        }
    }

    fn bind_names(&mut self, node: Node) {
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) {
            self.bound.insert(text(node, self.source).to_string());
        }
        for child in children(node) {
            self.bind_names(child);
        }
    }

    fn preloads(&mut self, node: Node) {
        if node.kind() == "string_fragment" {
            let value = text(node, self.source);
            if value.starts_with('.') {
                self.facts.preloads.push(value.to_string());
            }
        }
        for child in children(node) {
            self.preloads(child);
        }
    }

    /// Record `object.property` as a member access, typed when the object
    /// is the instance of the enclosing class.
    fn member(&mut self, object: Option<Node>, property: &str, owner: Owner) {
        match object.map(|o| o.kind()) {
            // A member of a literal is the language's own.
            Some(
                "array" | "string" | "template_string" | "number" | "regex" | "object" | "true"
                | "false" | "null",
            ) => return,
            Some("identifier") => {
                let name = object.map(|o| text(o, self.source)).unwrap_or_default();
                self.named_members
                    .push((owner, name.to_string(), property.to_string()));
                return;
            }
            _ => {}
        }
        let on_instance = object.is_some_and(|o| matches!(o.kind(), "this" | "super"));
        match self.this_type.clone().filter(|_| on_instance) {
            Some(class) => {
                self.facts
                    .owner(owner)
                    .typed
                    .insert((class, property.to_string()));
            }
            None => {
                self.facts.owner(owner).members.insert(property.to_string());
            }
        }
    }

    fn declarator(&mut self, node: Node, whole: Node) {
        let Some(name) = node.child_by_field_name("name") else {
            return self.walk(node, Owner::Top);
        };
        let value = node.child_by_field_name("value");
        // `const x = require("y")` binds an import rather than defining `x`.
        if let Some(spec) = value.and_then(|v| loaded_spec(v, self.source)) {
            let before = self.bindings.len();
            self.bind_pattern(name, spec);
            self.import_line(whole, before);
            return;
        }
        let mut names = Vec::new();
        self.pattern_names(name, &mut names);
        let Some(first) = names.first().cloned() else {
            return self.walk(node, Owner::Top);
        };
        let kind = match value.map(|v| v.kind()) {
            Some("arrow_function" | "function_expression" | "generator_function") => {
                DefKind::Function
            }
            _ => DefKind::Const,
        };
        let mut def = Def::new(first, kind, span(whole));
        def.aliases = names[1..].to_vec();
        let owner = self.facts.push(def);
        self.walk(node, owner);
        for name in &names {
            self.facts.owner(owner).refs.remove(name);
        }
    }

    /// Record `node` as the import statement behind the bindings added
    /// since `before`. An import that binds nothing runs for its side
    /// effects and stays file-level code.
    fn import_line(&mut self, node: Node, before: usize) {
        let bound = &self.bindings[before..];
        if bound.is_empty() {
            return;
        }
        let mut specs: Vec<String> = bound.iter().map(|b| b.spec.clone()).collect();
        specs.dedup();
        self.facts.imports.push(ImportLine {
            lines: span(node),
            names: bound.iter().map(|b| b.local.clone()).collect(),
            specs,
            structural: false,
        });
    }

    /// Names bound by a declaration pattern.
    fn pattern_names(&self, node: Node, out: &mut Vec<String>) {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                out.push(text(node, self.source).to_string());
            }
            "pair_pattern" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.pattern_names(value, out);
                }
            }
            "object_pattern"
            | "array_pattern"
            | "assignment_pattern"
            | "rest_pattern"
            | "object_assignment_pattern" => {
                for child in children(node) {
                    if child.is_named() {
                        self.pattern_names(child, out);
                        if node.kind().ends_with("assignment_pattern") {
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Bind the names of `const <pattern> = require(spec)`.
    fn bind_pattern(&mut self, pattern: Node, spec: &str) {
        match pattern.kind() {
            "identifier" => self.bindings.push(Binding {
                local: text(pattern, self.source).to_string(),
                spec: spec.to_string(),
                imported: None,
            }),
            "object_pattern" => {
                for child in children(pattern) {
                    let (imported, local) = match child.kind() {
                        "shorthand_property_identifier_pattern" => {
                            (text(child, self.source), text(child, self.source))
                        }
                        "pair_pattern" => {
                            let key = child.child_by_field_name("key");
                            let value = child.child_by_field_name("value");
                            match (key, value) {
                                (Some(key), Some(value)) => {
                                    (text(key, self.source), text(value, self.source))
                                }
                                _ => continue,
                            }
                        }
                        _ => continue,
                    };
                    self.bindings.push(Binding {
                        local: local.to_string(),
                        spec: spec.to_string(),
                        imported: Some(imported.to_string()),
                    });
                }
            }
            _ => {
                self.facts.top.uses.insert(spec.to_string());
            }
        }
    }

    fn import(&mut self, node: Node) {
        let Some(spec) = node
            .child_by_field_name("source")
            .and_then(|s| string_value(s, self.source))
        else {
            return;
        };
        // An imported asset is found by name when it changes.
        self.facts.top.string(spec);
        let clause = children(node)
            .into_iter()
            .find(|c| c.kind() == "import_clause");
        let Some(clause) = clause else {
            // Imported for its side effects.
            self.facts.top.uses.insert(spec.to_string());
            return;
        };
        for part in children(clause) {
            match part.kind() {
                "identifier" => self.bindings.push(Binding {
                    local: text(part, self.source).to_string(),
                    spec: spec.to_string(),
                    imported: Some("default".to_string()),
                }),
                "namespace_import" => {
                    if let Some(name) = part.named_child(0) {
                        self.bindings.push(Binding {
                            local: text(name, self.source).to_string(),
                            spec: spec.to_string(),
                            imported: None,
                        });
                    }
                }
                "named_imports" => {
                    for specifier in children(part) {
                        let Some(name) = specifier.child_by_field_name("name") else {
                            continue;
                        };
                        let local = specifier.child_by_field_name("alias").unwrap_or(name);
                        self.bindings.push(Binding {
                            local: text(local, self.source).to_string(),
                            spec: spec.to_string(),
                            imported: Some(text(name, self.source).to_string()),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn export(&mut self, node: Node) {
        if let Some(spec) = node
            .child_by_field_name("source")
            .and_then(|s| string_value(s, self.source))
        {
            self.facts.reexports.push(spec.to_string());
            return;
        }
        if let Some(declaration) = node.child_by_field_name("declaration") {
            let before = self.facts.defs.len();
            self.statement(declaration, node);
            let is_default = children(node).iter().any(|c| c.kind() == "default");
            if is_default {
                if let Some(def) = self.facts.defs.get_mut(before) {
                    if def.name != "default" {
                        def.aliases.push("default".to_string());
                    }
                }
            }
            return;
        }
        if let Some(value) = node.child_by_field_name("value") {
            let owner = self
                .facts
                .push(Def::new("default", DefKind::Const, span(node)));
            self.walk(value, owner);
            return;
        }
        // `export { a, b as c }`: `c` is another name for `b`.
        for clause in children(node) {
            if clause.kind() != "export_clause" {
                continue;
            }
            for specifier in children(clause) {
                let name = specifier.child_by_field_name("name");
                let alias = specifier.child_by_field_name("alias");
                match (name, alias) {
                    (Some(name), Some(alias)) => {
                        let mut def =
                            Def::new(text(alias, self.source), DefKind::Const, span(specifier));
                        def.refs.insert(text(name, self.source).to_string());
                        self.facts.push(def);
                    }
                    // Exported under its own name: importers find the
                    // definition itself.
                    (Some(_), None) => {}
                    _ => {}
                }
            }
        }
    }

    /// Record everything `node` mentions against `owner`.
    fn walk(&mut self, node: Node, owner: Owner) {
        match node.kind() {
            "comment" => {
                self.inert.push((node.start_byte(), node.end_byte()));
                return;
            }
            "identifier"
            | "property_identifier"
            | "type_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern" => {
                let name = text(node, self.source).to_string();
                if node.kind() == "shorthand_property_identifier_pattern" {
                    // `const { name } = value` reads a member.
                    self.facts.owner(owner).members.insert(name.clone());
                }
                self.facts.owner(owner).refs.insert(name);
                return;
            }
            "string_fragment" => {
                let value = text(node, self.source).to_string();
                if value.starts_with("./") || value.starts_with("../") {
                    self.facts.owner(owner).uses.insert(value.clone());
                }
                self.facts.owner(owner).string(&value);
                return;
            }
            "import_statement" => return self.import(node),
            "member_expression" => {
                if let Some(property) = node.child_by_field_name("property") {
                    let property = text(property, self.source).to_string();
                    self.member(node.child_by_field_name("object"), &property, owner);
                }
            }
            // `value["name"]`.
            "subscript_expression" => {
                let index = node.child_by_field_name("index");
                if let Some(name) = index.and_then(|i| string_value(i, self.source)) {
                    let name = name.to_string();
                    self.member(node.child_by_field_name("object"), &name, owner);
                }
            }
            // `setupFiles: ["./setup.ts"]`, `globalSetup: …` in a runner's
            // configuration.
            "pair" => {
                let key = node.child_by_field_name("key");
                let names_setup = key.is_some_and(|key| {
                    let key = text(key, self.source).to_ascii_lowercase();
                    key.contains("setup") || key.contains("teardown")
                });
                if let (true, Some(value)) = (names_setup, node.child_by_field_name("value")) {
                    self.preloads(value);
                }
            }
            // `const { other: local } = value`.
            "pair_pattern" => {
                if let Some(key) = node.child_by_field_name("key") {
                    let name = text(key, self.source).trim_matches(['"', '\'']).to_string();
                    self.facts.owner(owner).members.insert(name);
                }
            }
            // A function with its own `this`.
            "function_expression"
            | "function_declaration"
            | "generator_function"
            | "generator_function_declaration"
            | "method_definition"
            | "class"
            | "class_declaration"
            | "abstract_class_declaration"
                if self.this_type.is_some() =>
            {
                let outer = self.this_type.take();
                for child in children(node) {
                    self.walk(child, owner);
                }
                self.this_type = outer;
                return;
            }
            "call_expression" => {
                if let Some(spec) = loaded_spec(node, self.source) {
                    self.facts.owner(owner).uses.insert(spec.to_string());
                } else if node
                    .child_by_field_name("function")
                    .is_some_and(|function| {
                        function.kind() == "import" || text(function, self.source) == "require"
                    })
                {
                    // `import(name)`: whatever `name` turns out to be.
                    self.facts.open_loads = true;
                }
                if let Some(test) = self.test_call(node) {
                    self.has_tests = true;
                    let test_owner = self.facts.push(test);
                    for child in children(node) {
                        self.walk(child, test_owner);
                    }
                    return;
                }
            }
            _ => {}
        }
        for child in children(node) {
            self.walk(child, owner);
        }
    }

    /// `it("name", () => …)` and its variants.
    fn test_call(&self, node: Node) -> Option<Def> {
        let function = node.child_by_field_name("function")?;
        let root = callee_root(function, self.source)?;
        if !TEST_CALLS.contains(&root) {
            return None;
        }
        let arguments = node.child_by_field_name("arguments")?;
        let has_body = children(arguments).iter().any(|a| {
            matches!(
                a.kind(),
                "arrow_function" | "function_expression" | "identifier"
            )
        });
        if !has_body {
            return None;
        }
        let title = arguments
            .named_child(0)
            .map(|a| {
                text(a, self.source)
                    .trim_matches(|c| c == '"' || c == '\'' || c == '`')
                    .to_string()
            })
            .unwrap_or_default();
        Some(Def::new(title, DefKind::Test, span(node)))
    }

    /// Turn references to imported names into uses of the imported file and
    /// references to the name it has there.
    fn apply_bindings(&mut self) {
        let bindings = std::mem::take(&mut self.bindings);
        for (owner, name, member) in std::mem::take(&mut self.named_members) {
            let imported: Vec<&Binding> = bindings.iter().filter(|b| b.local == name).collect();
            let def = self.facts.owner(owner);
            if self.bound.contains(&name) {
                def.members.insert(member);
            } else if !imported.is_empty() {
                for binding in imported {
                    def.outside.insert((binding.spec.clone(), member.clone()));
                }
            } else if !GLOBALS.contains(&name.as_str()) {
                def.members.insert(member);
            }
        }
        let mut owners: Vec<Owner> = (0..self.facts.defs.len()).map(Owner::Def).collect();
        owners.push(Owner::Top);
        for owner in owners {
            let def = self.facts.owner(owner);
            for binding in &bindings {
                if !def.refs.contains(&binding.local) {
                    continue;
                }
                def.uses.insert(binding.spec.clone());
                if let Some(imported) = &binding.imported {
                    def.refs.insert(imported.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::related::facts::LineClass;

    #[test]
    fn tells_values_from_globals_literals_and_shadowed_imports() {
        let source = r#"import path from "node:path";
import { store } from "./store";

export function a(list, path) {
  // The parameter hides the import.
  return path.join(list);
}

export function b(list) {
  return [1].join(",") + Promise.resolve(list).then + store.get(1) + list.find(1);
}

export async function c(name) {
  const { open, close: shut } = await import(name);
  return obj["send"]();
}
"#;
        let facts = extract(Path::new("src/x.ts"), source);
        let def = |name: &str| facts.defs.iter().find(|d| d.name == name).unwrap();
        // `path` is bound in the file, so `path.join` may be anything.
        assert!(def("a").members.contains("join"));
        let b = def("b");
        assert!(!b.members.contains("join") && !b.members.contains("resolve"));
        assert!(b.members.contains("then") && b.members.contains("find"));
        assert!(b
            .outside
            .contains(&("./store".to_string(), "get".to_string())));
        let c = def("c");
        assert!(c.members.contains("open") && c.members.contains("close"));
        assert!(c.members.contains("send"));
        assert!(facts.open_loads);
    }

    #[test]
    fn extracts_declarations_and_import_uses() {
        let source = r#"import fmt, { parse as read } from "./parse";
import * as money from "@shop/money";
export * from "./types";
const legacy = require("./legacy");

// totals
export const total = (cart) => read(cart).reduce(money.add, 0);

export default function render(cart) {
  return fmt(legacy.wrap(cart));
}

export class Cart extends Base {
  items = [];
  constructor() { super(); audit(); }
  add(item) { return this.items.push(item); }
}
"#;
        let facts = extract(Path::new("src/cart.ts"), source);
        assert_eq!(facts.reexports, ["./types"]);
        let total = facts.defs.iter().find(|d| d.name == "total").unwrap();
        assert!(total.uses.contains("./parse"));
        assert!(total.uses.contains("@shop/money"));
        assert!(total.refs.contains("parse"));
        assert!(total.refs.contains("add"));
        let render = facts.defs.iter().find(|d| d.name == "render").unwrap();
        assert_eq!(render.aliases, ["default"]);
        assert!(render.refs.contains("default"));
        assert!(render.uses.contains("./legacy"));
        let cart = facts.defs.iter().find(|d| d.name == "Cart").unwrap();
        assert!(cart.refs.contains("audit"));
        let add = facts.defs.iter().find(|d| d.name == "add").unwrap();
        assert_eq!(add.kind, DefKind::Method);
        // `this.items` is a member of the class; `.push` is called on a
        // value of some other type.
        assert_eq!(add.owner.as_deref(), Some("Cart"));
        assert!(add
            .typed
            .contains(&("Cart".to_string(), "items".to_string())));
        assert!(add.members.contains("push"));
        assert!(cart.concrete);
        assert_eq!(cart.supers, ["Base"]);
        // `money.add` is a member of an import, `legacy.wrap` likewise.
        assert!(total
            .outside
            .contains(&("@shop/money".to_string(), "add".to_string())));
        assert!(!total.members.contains("add"));
        assert_eq!(facts.class_of(6), LineClass::Inert);
        assert_eq!(facts.class_of(1), LineClass::Import(0));
        assert_eq!(facts.imports[0].names, ["fmt", "read"]);
        assert_eq!(facts.class_of(3), LineClass::Top);
        assert_eq!(facts.class_of(4), LineClass::Import(2));
        assert!(!facts.is_test_file);
    }

    #[test]
    fn extracts_tests_and_mocks() {
        let source = r#"import { total } from "../src/cart";
vi.mock("../src/stock");

describe("cart", () => {
  beforeEach(() => reset());
  it("sums", () => {
    expect(total([])).toBe(0);
  });
  test.each([1, 2])("handles %i", (n) => {
    expect(n).toBeTruthy();
  });
});
"#;
        let facts = extract(Path::new("tests/cart.test.tsx"), source);
        assert!(facts.is_test_file);
        let tests: Vec<&str> = facts
            .defs
            .iter()
            .filter(|d| d.is_test())
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(tests, ["sums", "handles %i"]);
        let sums = &facts.defs[0];
        assert!(sums.uses.contains("../src/cart"));
        assert!(facts.top.uses.contains("../src/stock"));
        assert!(facts.top.refs.contains("reset"));
    }
}
