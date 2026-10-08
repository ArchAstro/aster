//! Go facts: package-level declarations, methods and `Test*` functions.
//! Imports are kept unresolved: the qualifier of an unaliased import is the
//! imported package's name, which only the index knows.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner, ANY_TYPE,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

const TEST_PREFIXES: &[&str] = &["Test", "Benchmark", "Fuzz", "Example"];

struct Walker<'a> {
    source: &'a str,
    facts: FileFacts,
    inert: Vec<(usize, usize)>,
    /// Embed patterns waiting for the declaration they precede.
    pending_embeds: Vec<String>,
    /// Names the function being read declares, with the type of each when
    /// it is certain and declared in this package.
    locals: HashMap<String, Option<String>>,
    /// Type parameters of the function being read.
    type_params: HashSet<String>,
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
        locals: HashMap::new(),
        type_params: HashSet::new(),
    };
    walker.facts.members = true;
    walker.facts.loads_by_import = true;
    walker.facts.qualified = true;
    walker.facts.tests_private = true;
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
                self.locals.clear();
                self.type_params.clear();
                self.declared(node);
                def.owner = node
                    .child_by_field_name("receiver")
                    .and_then(|receiver| self.receiver_type(receiver));
                let owner = self.facts.push(def);
                self.walk(node, owner);
                self.locals.clear();
                self.type_params.clear();
                self.facts.owner(owner).refs.remove(&name);
            }
            "type_declaration" | "const_declaration" | "var_declaration" => {
                let kind = if node.kind() == "type_declaration" {
                    DefKind::Type
                } else {
                    DefKind::Const
                };
                let mut specs = Vec::new();
                Self::specs(node, &mut specs);
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
                    if spec.kind() == "type_spec" {
                        self.shape(spec, &mut def);
                    }
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
    fn specs<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
        for child in children(node) {
            match child.kind() {
                "type_spec" | "type_alias" | "const_spec" | "var_spec" => out.push(child),
                "var_spec_list" | "const_spec_list" | "type_spec_list" => Self::specs(child, out),
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
            "identifier" | "type_identifier" | "package_identifier" => {
                let name = text(node, self.source).to_string();
                self.facts.owner(owner).refs.insert(name);
                return;
            }
            // A field or method name means nothing without its operand.
            "field_identifier" => return,
            // `pkg.Type`.
            "qualified_type" => {
                let package = node.child_by_field_name("package");
                let name = node.child_by_field_name("name");
                if let (Some(package), Some(name)) = (package, name) {
                    let package = text(package, self.source).to_string();
                    let name = text(name, self.source).to_string();
                    self.facts.owner(owner).refs.insert(package.clone());
                    self.facts.owner(owner).calls.insert((package, name));
                }
                return;
            }
            "interpreted_string_literal_content" | "raw_string_literal_content" => {
                let value = text(node, self.source).to_string();
                self.facts.owner(owner).string(&value);
                return;
            }
            "selector_expression" => self.selector(node, owner),
            _ => {}
        }
        for child in children(node) {
            self.walk(child, owner);
        }
    }

    /// Record `operand.field` as a member access, with the operand's type
    /// when the enclosing function declares it.
    fn selector(&mut self, node: Node, owner: Owner) {
        let (Some(operand), Some(field)) = (
            node.child_by_field_name("operand"),
            node.child_by_field_name("field"),
        ) else {
            return;
        };
        let field = text(field, self.source).to_string();
        let name = (operand.kind() == "identifier").then(|| text(operand, self.source));
        match name.map(|name| (name, self.locals.get(name))) {
            Some((_, Some(Some(declared)))) => {
                let declared = declared.clone();
                self.facts.owner(owner).typed.insert((declared, field));
            }
            // A name the function does not declare: a package (`pkg.Name`)
            // or a package-level value. The index knows the file's
            // packages and decides.
            Some((name, None)) => {
                let name = name.to_string();
                self.facts.owner(owner).calls.insert((name, field));
            }
            _ => {
                self.facts.owner(owner).members.insert(field);
            }
        }
    }

    /// The type a type expression names, when it is one this package can
    /// declare: `T` or `*T`, and not a type parameter.
    fn named(&self, node: Node) -> Option<String> {
        match node.kind() {
            "type_identifier" => {
                let name = text(node, self.source);
                (!self.type_params.contains(name)).then(|| name.to_string())
            }
            "pointer_type" | "parenthesized_type" => self.named(node.named_child(0)?),
            _ => None,
        }
    }

    /// The type a method's receiver names, through `*` and type arguments.
    fn receiver_type(&self, receiver: Node) -> Option<String> {
        let mut node = children(receiver)
            .into_iter()
            .find(|c| c.kind() == "parameter_declaration")?
            .child_by_field_name("type")?;
        loop {
            match node.kind() {
                "type_identifier" => return Some(text(node, self.source).to_string()),
                "pointer_type" | "parenthesized_type" => node = node.named_child(0)?,
                "generic_type" => node = node.child_by_field_name("type")?,
                _ => return None,
            }
        }
    }

    /// The type a value certainly has: `T{…}` or `&T{…}`.
    fn literal_type(&self, value: Node) -> Option<String> {
        match value.kind() {
            "composite_literal" => self.named(value.child_by_field_name("type")?),
            "unary_expression" if text(value, self.source).starts_with('&') => {
                self.literal_type(value.child_by_field_name("operand")?)
            }
            _ => None,
        }
    }

    /// Note that the function declares `name`, with its type when known.
    /// Two declarations that disagree leave the type unknown.
    fn declare(&mut self, name: Node, declared: Option<String>) {
        if name.kind() != "identifier" {
            return;
        }
        let name = text(name, self.source).to_string();
        match self.locals.get_mut(&name) {
            Some(known) if *known != declared => *known = None,
            Some(_) => {}
            None => {
                self.locals.insert(name, declared);
            }
        }
    }

    /// Collect every name a function declares, before its body is read.
    fn declared(&mut self, node: Node) {
        match node.kind() {
            "type_parameter_declaration" => {
                let mut cursor = node.walk();
                for name in node.children_by_field_name("name", &mut cursor) {
                    self.type_params.insert(text(name, self.source).to_string());
                }
            }
            "parameter_declaration"
            | "variadic_parameter_declaration"
            | "var_spec"
            | "const_spec" => {
                let variadic = node.kind() == "variadic_parameter_declaration";
                let mut declared = node
                    .child_by_field_name("type")
                    .filter(|_| !variadic)
                    .and_then(|t| self.named(t));
                let mut cursor = node.walk();
                let names: Vec<Node> = node.children_by_field_name("name", &mut cursor).collect();
                if node.kind() == "var_spec" && node.child_by_field_name("type").is_none() {
                    declared = node
                        .child_by_field_name("value")
                        .filter(|values| values.named_child_count() == 1 && names.len() == 1)
                        .and_then(|values| self.literal_type(values.named_child(0)?));
                }
                for name in names {
                    self.declare(name, declared.clone());
                }
            }
            "short_var_declaration" => {
                let side = |field: &str| -> Vec<Node> {
                    node.child_by_field_name(field)
                        .map(|list| {
                            let mut cursor = list.walk();
                            list.named_children(&mut cursor).collect()
                        })
                        .unwrap_or_default()
                };
                let (left, right) = (side("left"), side("right"));
                for (index, name) in left.iter().enumerate() {
                    let declared = (left.len() == right.len())
                        .then(|| self.literal_type(right[index]))
                        .flatten();
                    self.declare(*name, declared);
                }
            }
            "range_clause" | "receive_statement" => {
                if let Some(left) = node.child_by_field_name("left") {
                    for name in children(left) {
                        self.declare(name, None);
                    }
                }
            }
            "type_switch_statement" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    for name in children(alias) {
                        self.declare(name, None);
                    }
                }
            }
            _ => {}
        }
        for child in children(node) {
            self.declared(child);
        }
    }

    /// Whether a declared type keeps its own method set, and the types it
    /// embeds.
    fn shape(&self, spec: Node, def: &mut Def) {
        let Some(body) = spec.child_by_field_name("type") else {
            return;
        };
        match body.kind() {
            "struct_type" => {
                def.concrete = true;
                let mut fields = Vec::new();
                collect(body, "field_declaration", &mut fields);
                for field in fields {
                    if field.child_by_field_name("name").is_some() {
                        continue;
                    }
                    let embedded = field
                        .child_by_field_name("type")
                        .and_then(|t| self.named(t))
                        .unwrap_or_else(|| ANY_TYPE.to_string());
                    def.supers.push(embedded);
                }
            }
            // A defined type over another named type may be an interface.
            "type_identifier" | "qualified_type" | "interface_type" | "generic_type" => {}
            _ => def.concrete = true,
        }
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
        // `money.Sum` is left for the index to tell from a member access;
        // `c.Items` is a member of the receiver's type.
        assert!(total.refs.contains("money") && !total.refs.contains("Sum"));
        assert!(total
            .calls
            .contains(&("money".to_string(), "Sum".to_string())));
        assert!(total
            .typed
            .contains(&("Cart".to_string(), "Items".to_string())));
        assert_eq!(total.owner.as_deref(), Some("Cart"));
        let cart = facts.defs.iter().find(|d| d.name == "Cart").unwrap();
        assert!(cart.concrete);
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
