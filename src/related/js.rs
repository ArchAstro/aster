//! TypeScript and JavaScript facts: top-level declarations, class members,
//! import bindings and test blocks.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner,
};
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
}

pub fn extract(path: &Path, source: &str) -> FileFacts {
    let mut facts = FileFacts::new(Family::Js);
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
    };
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
        let class = self
            .facts
            .push(Def::new(name.clone(), DefKind::Type, span(whole)));
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
                        let owner = self.facts.push(Def::new(
                            member_name.clone(),
                            DefKind::Method,
                            span(member),
                        ));
                        self.walk(member, owner);
                        self.facts.owner(owner).refs.remove(&member_name);
                    }
                    _ => self.walk(member, class),
                }
            }
        }
        self.facts.owner(class).refs.remove(&name);
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
            "call_expression" => {
                if let Some(spec) = loaded_spec(node, self.source) {
                    self.facts.owner(owner).uses.insert(spec.to_string());
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
