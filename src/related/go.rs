//! Go facts: package-level declarations, methods and `Test*` functions.
//! Imports are kept unresolved: the qualifier of an unaliased import is the
//! imported package's name, which only the index knows.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner,
};
use std::path::Path;
use tree_sitter::Node;

const TEST_PREFIXES: &[&str] = &["Test", "Benchmark", "Fuzz", "Example"];

struct Walker<'a> {
    source: &'a str,
    facts: FileFacts,
    inert: Vec<(usize, usize)>,
    /// Embed patterns waiting for the declaration they precede.
    pending_embeds: Vec<String>,
}

pub fn extract(path: &Path, source: &str) -> FileFacts {
    let mut facts = FileFacts::new(Family::Go);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    facts.is_test_file = name.ends_with("_test.go");
    let Some(tree) = parse(tree_sitter_go::LANGUAGE.into(), source) else {
        facts.parse_error = true;
        return facts;
    };
    facts.parse_error = tree.root_node().has_error();
    let mut walker = Walker {
        source,
        facts,
        inert: Vec::new(),
        pending_embeds: Vec::new(),
    };
    for child in children(tree.root_node()) {
        let before = walker.facts.defs.len();
        walker.declaration(child);
        if child.kind() != "comment" {
            let owner = if walker.facts.defs.len() > before {
                Owner::Def(before)
            } else {
                Owner::Top
            };
            for pattern in walker.pending_embeds.drain(..) {
                walker.facts.embeds.push((pattern, owner));
            }
        }
    }
    let mut facts = walker.facts;
    facts.classify_lines(source, &walker.inert);
    facts
}

impl Walker<'_> {
    fn declaration(&mut self, node: Node) {
        match node.kind() {
            "comment" => {
                let comment = text(node, self.source);
                if let Some(patterns) = comment.strip_prefix("//go:embed") {
                    self.pending_embeds.extend(
                        shell_words::split(patterns)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| p.trim_start_matches("all:").to_string()),
                    );
                } else if !comment.starts_with("//go:") {
                    self.inert.push((node.start_byte(), node.end_byte()));
                }
            }
            "package_clause" => {
                if let Some(name) = node.named_child(0) {
                    self.facts.modules.push(text(name, self.source).to_string());
                }
            }
            "import_declaration" => {
                self.facts.imports.push(ImportLine {
                    lines: span(node),
                    structural: true,
                    ..ImportLine::default()
                });
                self.imports(node);
            }
            "function_declaration" | "method_declaration" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let name = text(name, self.source).to_string();
                let is_method = node.kind() == "method_declaration";
                let kind = if is_method {
                    DefKind::Method
                } else if self.facts.is_test_file
                    && TEST_PREFIXES.iter().any(|p| name.starts_with(p))
                    && name != "TestMain"
                {
                    DefKind::Test
                } else {
                    DefKind::Function
                };
                let mut def = Def::new(name.clone(), kind, span(node));
                def.callback = !is_method && matches!(name.as_str(), "init" | "main" | "TestMain");
                let owner = self.facts.push(def);
                self.walk(node, owner);
                self.facts.owner(owner).refs.remove(&name);
            }
            "type_declaration" | "const_declaration" | "var_declaration" => {
                let kind = if node.kind() == "type_declaration" {
                    DefKind::Type
                } else {
                    DefKind::Const
                };
                let mut specs = Vec::new();
                self.specs(node, &mut specs);
                let single = specs.len() == 1;
                for spec in specs {
                    let mut names: Vec<String> = Vec::new();
                    let mut cursor = spec.walk();
                    for name in spec.children_by_field_name("name", &mut cursor) {
                        names.push(text(name, self.source).to_string());
                    }
                    let Some(first) = names.first().cloned() else {
                        self.walk(spec, Owner::Top);
                        continue;
                    };
                    let lines = if single { span(node) } else { span(spec) };
                    let mut def = Def::new(first, kind, lines);
                    def.aliases = names[1..].to_vec();
                    let owner = self.facts.push(def);
                    self.walk(spec, owner);
                    for name in &names {
                        self.facts.owner(owner).refs.remove(name);
                    }
                }
            }
            _ => self.walk(node, Owner::Top),
        }
    }

    /// The `*_spec` nodes of a declaration, through parenthesised groups.
    fn specs<'t>(&self, node: Node<'t>, out: &mut Vec<Node<'t>>) {
        for child in children(node) {
            match child.kind() {
                "type_spec" | "type_alias" | "const_spec" | "var_spec" => out.push(child),
                "var_spec_list" | "const_spec_list" | "type_spec_list" => self.specs(child, out),
                _ => {}
            }
        }
    }

    fn imports(&mut self, node: Node) {
        if node.kind() == "import_spec" {
            let Some(path) = node.child_by_field_name("path") else {
                return;
            };
            let path = text(path, self.source).trim_matches(|c| c == '"' || c == '`');
            let alias = node
                .child_by_field_name("name")
                .map(|n| text(n, self.source).to_string());
            // A blank or dot import has no qualifier to look for, so a
            // change to it stays file-level. The qualifier of an unaliased
            // import is guessed from its path; a wrong guess matches no
            // definition, which also counts as file-level.
            if !matches!(alias.as_deref(), Some("_" | ".")) {
                let guess = path
                    .rsplit('/')
                    .find(|s| !(s.len() > 1 && s.starts_with('v') && s[1..].parse::<u32>().is_ok()))
                    .unwrap_or(path);
                self.facts.imports.push(ImportLine {
                    lines: span(node),
                    names: vec![alias.clone().unwrap_or_else(|| guess.to_string())],
                    specs: Vec::new(),
                    structural: false,
                });
            }
            self.facts.go_imports.push((alias, path.to_string()));
            return;
        }
        for child in children(node) {
            self.imports(child);
        }
    }

    fn walk(&mut self, node: Node, owner: Owner) {
        match node.kind() {
            "comment" => {
                self.inert.push((node.start_byte(), node.end_byte()));
                return;
            }
            "identifier" | "field_identifier" | "type_identifier" | "package_identifier" => {
                let name = text(node, self.source).to_string();
                self.facts.owner(owner).refs.insert(name);
                return;
            }
            "interpreted_string_literal_content" | "raw_string_literal_content" => {
                let value = text(node, self.source).to_string();
                self.facts.owner(owner).string(&value);
                return;
            }
            _ => {}
        }
        for child in children(node) {
            self.walk(child, owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_declarations_methods_and_tests() {
        let source = r#"package cart

import (
	"fmt"
	money "example.com/shop/money"
	_ "example.com/shop/hooks"
)

//go:embed prices.json
var prices []byte

type Cart struct{ Items []Item }

// Total sums the cart.
func (c *Cart) Total() int { return money.Sum(c.Items) }

func init() { fmt.Println("x") }

const (
	Limit = 10
	Burst = 20
)
"#;
        let facts = extract(Path::new("cart/cart.go"), source);
        assert_eq!(facts.modules, ["cart"]);
        assert_eq!(facts.embeds.len(), 1);
        assert_eq!(facts.embeds[0].0, "prices.json");
        assert!(matches!(facts.embeds[0].1, Owner::Def(i) if facts.defs[i].name == "prices"));
        assert_eq!(
            facts.go_imports,
            [
                (None, "fmt".to_string()),
                (
                    Some("money".to_string()),
                    "example.com/shop/money".to_string()
                ),
                (Some("_".to_string()), "example.com/shop/hooks".to_string()),
            ]
        );
        let total = facts.defs.iter().find(|d| d.name == "Total").unwrap();
        assert_eq!(total.kind, DefKind::Method);
        assert!(total.refs.contains("money") && total.refs.contains("Sum"));
        assert!(facts
            .defs
            .iter()
            .any(|d| d.name == "Cart" && d.kind == DefKind::Type));
        assert!(facts.defs.iter().any(|d| d.name == "init" && d.callback));
        assert!(facts.defs.iter().any(|d| d.name == "Limit"));
        assert!(facts.defs.iter().any(|d| d.name == "Burst"));

        let tests = extract(
            Path::new("cart/cart_test.go"),
            "package cart\nfunc TestTotal(t *testing.T) { helper(t) }\nfunc helper(t *testing.T) {}\nfunc TestMain(m *testing.M) {}\n",
        );
        assert!(tests.is_test_file);
        assert!(tests.defs[0].is_test());
        assert!(!tests.defs[1].is_test());
        assert!(tests.defs[2].callback);
    }
}
