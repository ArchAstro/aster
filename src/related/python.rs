//! Python facts: module-level functions, classes, methods and assignments,
//! import bindings and pytest tests.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner,
};
use std::path::Path;
use tree_sitter::Node;

/// A name bound by an import, with the modules it may come from.
struct Binding {
    local: String,
    specs: Vec<String>,
    imported: Option<String>,
}

struct Walker<'a> {
    source: &'a str,
    facts: FileFacts,
    bindings: Vec<Binding>,
    inert: Vec<(usize, usize)>,
}

pub fn extract(path: &Path, source: &str) -> FileFacts {
    let mut facts = FileFacts::new(Family::Python);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    facts.is_test_file =
        (name.starts_with("test_") && name.ends_with(".py")) || name.ends_with("_test.py");
    let Some(tree) = parse(tree_sitter_python::LANGUAGE.into(), source) else {
        facts.parse_error = true;
        return facts;
    };
    facts.parse_error = tree.root_node().has_error();
    let mut walker = Walker {
        source,
        facts,
        bindings: Vec::new(),
        inert: Vec::new(),
    };
    for child in children(tree.root_node()) {
        walker.statement(child, child, None);
    }
    walker.apply_bindings();
    let mut facts = walker.facts;
    // `__init__.py` hands its imports on to whoever imports the package.
    if name == "__init__.py" {
        let mut reexports: Vec<String> = facts.top.uses.iter().cloned().collect();
        reexports.extend(facts.uses_all.iter().cloned());
        facts.reexports = reexports;
    }
    facts.classify_lines(source, &walker.inert);
    facts
}

/// All dotted prefixes of `module`: `a.b.c` gives `a`, `a.b`, `a.b.c`.
/// Leading dots of a relative import stay on every prefix.
fn prefixes(module: &str) -> Vec<String> {
    let dots: String = module.chars().take_while(|c| *c == '.').collect();
    let rest = &module[dots.len()..];
    if rest.is_empty() {
        return vec![dots];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for part in rest.split('.') {
        if !current.is_empty() {
            current.push('.');
        }
        current.push_str(part);
        out.push(format!("{dots}{current}"));
    }
    out
}

fn join(module: &str, name: &str) -> String {
    if module.ends_with('.') {
        format!("{module}{name}")
    } else {
        format!("{module}.{name}")
    }
}

impl Walker<'_> {
    /// A module-level or class-level statement. `whole` includes decorators;
    /// `class` is the enclosing class definition, if any.
    fn statement(&mut self, node: Node, whole: Node, class: Option<Owner>) {
        let outer = class.unwrap_or(Owner::Top);
        match node.kind() {
            "comment" => self.inert.push((node.start_byte(), node.end_byte())),
            "import_statement" | "import_from_statement" if class.is_none() => {
                let before = self.bindings.len();
                self.import(node);
                let bound = &self.bindings[before..];
                // A wildcard import binds names this walk cannot list.
                if !bound.is_empty() {
                    self.facts.imports.push(ImportLine {
                        lines: span(node),
                        names: bound.iter().map(|b| b.local.clone()).collect(),
                        specs: bound.iter().flat_map(|b| b.specs.clone()).collect(),
                        structural: false,
                    });
                }
            }
            "decorated_definition" => {
                if let Some(definition) = node.child_by_field_name("definition") {
                    let before = self.facts.defs.len();
                    self.statement(definition, node, class);
                    // Decorators belong to the definition they wrap.
                    let owner = if self.facts.defs.len() > before {
                        Owner::Def(before)
                    } else {
                        outer
                    };
                    for child in children(node) {
                        if child.kind() == "decorator" {
                            self.walk(child, owner);
                        }
                    }
                }
            }
            "function_definition" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let name = text(name, self.source).to_string();
                // Dunder methods run when the class is used, not by name.
                if class.is_some() && name.starts_with("__") && name.ends_with("__") {
                    return self.walk(node, outer);
                }
                let kind = if self.facts.is_test_file && name.starts_with("test") {
                    DefKind::Test
                } else if class.is_some() {
                    DefKind::Method
                } else {
                    DefKind::Function
                };
                let owner = self.facts.push(Def::new(name.clone(), kind, span(whole)));
                self.walk(node, owner);
                self.facts.owner(owner).refs.remove(&name);
            }
            "class_definition" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let name = text(name, self.source).to_string();
                let owner = self
                    .facts
                    .push(Def::new(name.clone(), DefKind::Type, span(whole)));
                for child in children(node) {
                    if child.kind() == "block" {
                        for member in children(child) {
                            self.statement(member, member, Some(owner));
                        }
                    } else {
                        self.walk(child, owner);
                    }
                }
                self.facts.owner(owner).refs.remove(&name);
            }
            "expression_statement" if class.is_none() => {
                let assigned = node
                    .named_child(0)
                    .filter(|a| a.kind() == "assignment")
                    .and_then(|a| a.child_by_field_name("left"))
                    .filter(|left| left.kind() == "identifier")
                    .map(|left| text(left, self.source).to_string());
                match assigned {
                    Some(name) if name != "__all__" => {
                        let owner =
                            self.facts
                                .push(Def::new(name.clone(), DefKind::Const, span(node)));
                        self.walk(node, owner);
                        self.facts.owner(owner).refs.remove(&name);
                    }
                    _ => self.walk(node, outer),
                }
            }
            _ => self.walk(node, outer),
        }
    }

    fn walk(&mut self, node: Node, owner: Owner) {
        match node.kind() {
            "comment" => {
                self.inert.push((node.start_byte(), node.end_byte()));
                return;
            }
            "import_statement" | "import_from_statement" | "future_import_statement" => {
                return self.import(node);
            }
            "identifier" => {
                let name = text(node, self.source).to_string();
                if matches!(name.as_str(), "import_module" | "__import__") {
                    self.facts.dynamic = Some("imports modules by a computed name");
                }
                self.facts.owner(owner).refs.insert(name);
                return;
            }
            "string_content" => {
                let value = text(node, self.source).to_string();
                self.facts.owner(owner).string(&value);
                return;
            }
            "string" => {
                // A docstring is an expression statement holding one string.
                let docstring = node.parent().is_some_and(|p| {
                    p.kind() == "expression_statement" && p.named_child_count() == 1
                });
                if docstring {
                    self.inert.push((node.start_byte(), node.end_byte()));
                    return;
                }
            }
            _ => {}
        }
        for child in children(node) {
            self.walk(child, owner);
        }
    }

    fn import(&mut self, node: Node) {
        if node.kind() == "import_statement" {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let (module, alias) = self.aliased(name);
                match alias {
                    // `import a.b as c` binds the leaf module.
                    Some(alias) => self.bindings.push(Binding {
                        local: alias,
                        specs: vec![module],
                        imported: None,
                    }),
                    // `import a.b` binds `a` and loads every level.
                    None => self.bindings.push(Binding {
                        local: module.split('.').next().unwrap_or(&module).to_string(),
                        specs: prefixes(&module),
                        imported: None,
                    }),
                }
            }
            return;
        }
        let Some(module) = node.child_by_field_name("module_name") else {
            return;
        };
        let module = text(module, self.source).to_string();
        if children(node).iter().any(|c| c.kind() == "wildcard_import") {
            self.facts.uses_all.push(module);
            return;
        }
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            let (imported, alias) = self.aliased(name);
            // `from a import b`: `b` is a name in `a` or the module `a.b`.
            self.bindings.push(Binding {
                local: alias.unwrap_or_else(|| imported.clone()),
                specs: vec![module.clone(), join(&module, &imported)],
                imported: Some(imported),
            });
        }
    }

    /// `(name, alias)` of `x` or `x as y`.
    fn aliased(&self, node: Node) -> (String, Option<String>) {
        if node.kind() == "aliased_import" {
            let name = node
                .child_by_field_name("name")
                .map(|n| text(n, self.source).to_string())
                .unwrap_or_default();
            let alias = node
                .child_by_field_name("alias")
                .map(|n| text(n, self.source).to_string());
            (name, alias)
        } else {
            (text(node, self.source).to_string(), None)
        }
    }

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
                def.uses.extend(binding.specs.iter().cloned());
                if let Some(imported) = &binding.imported {
                    def.refs.insert(imported.clone());
                }
            }
        }
        // An import nothing references still runs the module.
        for binding in &bindings {
            self.facts.top.uses.extend(binding.specs.iter().cloned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::related::facts::LineClass;

    #[test]
    fn extracts_definitions_and_import_uses() {
        let source = r#"import shop.money as money
from .stock import hold, release as free
from shop.legacy import *

LIMIT = 10

@traced
def total(cart):
    """Sum the cart."""
    return money.add(hold(cart), LIMIT)

class Cart(Base):
    def __init__(self):
        free(self)

    def add(self, item):
        return item
"#;
        let facts = extract(Path::new("shop/cart.py"), source);
        assert_eq!(facts.uses_all, ["shop.legacy"]);
        let total = facts.defs.iter().find(|d| d.name == "total").unwrap();
        assert!(total.uses.contains("shop.money"));
        assert!(total.uses.contains(".stock") && total.uses.contains(".stock.hold"));
        assert!(total.refs.contains("traced") && total.refs.contains("LIMIT"));
        assert_eq!(total.lines.0, 7);
        assert_eq!(facts.class_of(9), LineClass::Inert);
        let cart = facts.defs.iter().find(|d| d.name == "Cart").unwrap();
        assert!(cart.refs.contains("release"));
        let add = facts.defs.iter().find(|d| d.name == "add").unwrap();
        assert_eq!(add.kind, DefKind::Method);
        assert!(facts.defs.iter().any(|d| d.name == "LIMIT"));
    }

    #[test]
    fn extracts_tests_and_dynamic_imports() {
        let source = "from shop.cart import total\n\ndef test_total(cart):\n    assert total(cart) == 0\n\nclass TestCart:\n    def test_add(self):\n        pass\n\nplugin = importlib.import_module(name)\n";
        let facts = extract(Path::new("tests/test_cart.py"), source);
        assert!(facts.is_test_file);
        let tests: Vec<&str> = facts
            .defs
            .iter()
            .filter(|d| d.is_test())
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(tests, ["test_total", "test_add"]);
        assert!(facts.defs[0].uses.contains("shop.cart.total"));
        assert!(facts.defs[0].refs.contains("cart"));
        assert!(facts.dynamic.is_some());
    }
}
