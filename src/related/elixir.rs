//! Elixir facts: functions and macros per module, the modules each one
//! mentions (through `alias`), and ExUnit tests.

use super::facts::{
    children, parse, span, text, Def, DefKind, Family, FileFacts, ImportLine, Owner, WILDCARD,
};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::Node;

const DEFINERS: &[&str] = &[
    "def",
    "defp",
    "defmacro",
    "defmacrop",
    "defguard",
    "defguardp",
    "defdelegate",
    "defn",
    "defnp",
];
const MODULE_DEFINERS: &[&str] = &["defmodule", "defprotocol", "defimpl"];
const TEST_MACROS: &[&str] = &["test", "property", "feature"];
/// Attributes that never change behaviour: documentation and types.
const INERT_ATTRIBUTES: &[&str] = &[
    "doc",
    "moduledoc",
    "typedoc",
    "spec",
    "type",
    "typep",
    "opaque",
    "callback",
    "macrocallback",
];
/// Callbacks the runtime invokes; nothing calls them by name.
const CALLBACKS: &[&str] = &[
    "init",
    "handle_call",
    "handle_cast",
    "handle_info",
    "handle_continue",
    "terminate",
    "code_change",
    "child_spec",
    "start_link",
    "start",
    "mount",
    "render",
    "update",
    "handle_event",
    "handle_params",
    "handle_async",
    "handle_in",
    "handle_out",
    "join",
    "call",
    "perform",
    "__using__",
    "__before_compile__",
    "__after_compile__",
];

struct Walker<'a> {
    source: &'a str,
    facts: FileFacts,
    aliases: HashMap<String, String>,
    inert: Vec<(usize, usize)>,
    test_file: bool,
    /// The protocol and type of the `defimpl` being walked.
    impl_of: Vec<String>,
    /// Path prefixes and module prefixes of the enclosing route scopes.
    route_scopes: Vec<(String, Option<String>)>,
}

#[derive(Clone, Copy)]
struct Scope {
    owner: Owner,
    /// Inside a `defimpl`, or the definition is marked `@impl`.
    in_impl: bool,
    /// Directly inside a module body or the file, where definitions live.
    module_level: bool,
    /// Inside the parameter patterns of a function head. A module named
    /// there is matched against, not called: `def limit(Api.List), do: 10`
    /// runs nothing of `Api.List`.
    pattern: bool,
}

pub fn extract(path: &Path, source: &str) -> FileFacts {
    let mut facts = FileFacts::new(Family::Elixir);
    facts.qualified = true;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    facts.is_test_file = name.ends_with("_test.exs");
    let Some(tree) = parse(tree_sitter_elixir::LANGUAGE.into(), source) else {
        facts.parse_error = true;
        return facts;
    };
    facts.parse_error = tree.root_node().has_error();
    let mut walker = Walker {
        source,
        test_file: facts.is_test_file,
        facts,
        aliases: HashMap::new(),
        inert: Vec::new(),
        impl_of: Vec::new(),
        route_scopes: Vec::new(),
    };
    walker.collect_aliases(tree.root_node());
    let scope = Scope {
        owner: Owner::Top,
        in_impl: false,
        module_level: true,
        pattern: false,
    };
    walker.block(tree.root_node(), scope, None);
    let mut facts = walker.facts;
    facts.classify_lines(source, &walker.inert);
    facts
}

/// Whether a remote call lists modules at run time: every module of an
/// application (`:application.get_key(app, :modules)`,
/// `Application.spec(app, :modules)`) or every module loaded
/// (`:code.all_loaded()`).
fn lists_modules(receiver: &str, function: &str, call: Node, source: &str) -> bool {
    match (receiver, function) {
        (":code", "all_loaded" | "all_available") => true,
        (":application", "get_key") | ("Application", "spec") => arguments(call)
            .iter()
            .any(|arg| text(*arg, source) == ":modules"),
        _ => false,
    }
}

fn call_name<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    if node.kind() != "call" {
        return None;
    }
    let target = node.child_by_field_name("target")?;
    (target.kind() == "identifier").then(|| text(target, source))
}

fn arguments(node: Node) -> Vec<Node> {
    children(node)
        .into_iter()
        .find(|c| c.kind() == "arguments")
        .map(|args| {
            children(args)
                .into_iter()
                .filter(|c| c.is_named())
                .collect()
        })
        .unwrap_or_default()
}

impl Walker<'_> {
    /// Record every `alias` directive in the file, ignoring lexical scope.
    fn collect_aliases(&mut self, node: Node) {
        if call_name(node, self.source) == Some("alias") {
            let args = arguments(node);
            let before: Vec<String> = self.aliases.values().cloned().collect();
            self.collect_alias(&args);
            let specs = self
                .aliases
                .values()
                .filter(|full| !before.contains(full))
                .cloned()
                .collect();
            self.facts.imports.push(ImportLine {
                lines: span(node),
                specs,
                ..ImportLine::default()
            });
            return;
        }
        for child in children(node) {
            self.collect_aliases(child);
        }
    }

    fn collect_alias(&mut self, args: &[Node]) {
        {
            if let Some(first) = args.first() {
                let custom = args.iter().skip(1).find_map(|arg| self.as_option(*arg));
                match first.kind() {
                    "alias" => {
                        let full = self.expand(text(*first, self.source));
                        let short = custom.unwrap_or_else(|| {
                            full.rsplit('.').next().unwrap_or(&full).to_string()
                        });
                        self.aliases.insert(short, full);
                    }
                    "dot" => {
                        let prefix = first
                            .child_by_field_name("left")
                            .map(|n| self.expand(text(n, self.source)));
                        let tuple = first.child_by_field_name("right");
                        if let (Some(prefix), Some(tuple)) = (prefix, tuple) {
                            for item in children(tuple) {
                                if item.kind() == "alias" {
                                    let tail = text(item, self.source);
                                    let short = tail.rsplit('.').next().unwrap_or(tail);
                                    self.aliases
                                        .insert(short.to_string(), format!("{prefix}.{tail}"));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The module named by an `as:` option.
    fn as_option(&self, node: Node) -> Option<String> {
        if node.kind() != "keywords" {
            return None;
        }
        children(node).into_iter().find_map(|pair| {
            let key = pair.child_by_field_name("key")?;
            let value = pair.child_by_field_name("value")?;
            (text(key, self.source).trim().trim_end_matches(':') == "as")
                .then(|| text(value, self.source).to_string())
        })
    }

    /// Replace a leading alias with the module it stands for.
    fn expand(&self, name: &str) -> String {
        let (head, rest) = match name.split_once('.') {
            Some((head, rest)) => (head, Some(rest)),
            None => (name, None),
        };
        match (self.aliases.get(head), rest) {
            (Some(full), Some(rest)) => format!("{full}.{rest}"),
            (Some(full), None) => full.clone(),
            _ => name.to_string(),
        }
    }

    fn mention(&mut self, owner: Owner, module: &str) {
        if module.starts_with("__") {
            return;
        }
        let full = self.expand(module);
        self.facts.owner(owner).uses.insert(full);
    }

    /// Walk the statements of a module body or the file.
    fn block(&mut self, node: Node, scope: Scope, module: Option<&str>) {
        let mut pending_impl = false;
        for child in children(node) {
            if child.kind() == "unary_operator" && text(child, self.source).starts_with('@') {
                let attribute = child
                    .child_by_field_name("operand")
                    .and_then(|operand| call_name(operand, self.source));
                if attribute == Some("impl") {
                    pending_impl = true;
                    self.inert.push((child.start_byte(), child.end_byte()));
                    continue;
                }
                if attribute.is_some_and(|a| INERT_ATTRIBUTES.contains(&a)) {
                    self.inert.push((child.start_byte(), child.end_byte()));
                    continue;
                }
            }
            let is_def = call_name(child, self.source).is_some_and(|n| DEFINERS.contains(&n));
            self.node(
                child,
                Scope {
                    in_impl: scope.in_impl || (pending_impl && is_def),
                    ..scope
                },
                module,
            );
            if child.kind() != "comment" {
                pending_impl = false;
            }
        }
    }

    fn node(&mut self, node: Node, scope: Scope, module: Option<&str>) {
        match node.kind() {
            "comment" => {
                self.inert.push((node.start_byte(), node.end_byte()));
                return;
            }
            "alias" => {
                if !scope.pattern {
                    self.mention(scope.owner, text(node, self.source));
                }
                return;
            }
            "identifier" => {
                let name = text(node, self.source).to_string();
                self.facts.owner(scope.owner).refs.insert(name);
                return;
            }
            "atom" | "quoted_atom" => {
                let name = text(node, self.source).trim_start_matches(':').to_string();
                if name
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_?!".contains(c))
                {
                    self.facts.owner(scope.owner).atoms.insert(name);
                }
                return;
            }
            "quoted_content" => {
                let content = text(node, self.source).to_string();
                self.facts.owner(scope.owner).string(&content);
                return;
            }
            "string" | "sigil" => {
                if let Some(path) = self.path_literal(node) {
                    self.facts.owner(scope.owner).paths.push(path);
                }
                for child in children(node) {
                    self.node(child, scope, module);
                }
                return;
            }
            "call" => {}
            _ => {
                for child in children(node) {
                    self.node(child, scope, module);
                }
                return;
            }
        }

        let Some(name) = call_name(node, self.source) else {
            let target = node.child_by_field_name("target");
            let parts = target.filter(|t| t.kind() == "dot").and_then(|dot| {
                Some((
                    dot.child_by_field_name("left")?,
                    dot.child_by_field_name("right")?,
                ))
            });
            match parts {
                Some((left, right)) if right.kind() == "identifier" => {
                    let function = text(right, self.source).to_string();
                    let receiver = text(left, self.source);
                    if lists_modules(receiver, &function, node, self.source) {
                        self.facts.owner(scope.owner).reflects = Some("lists modules at run time");
                    }
                    if left.kind() == "alias" {
                        // `Module.function(...)`.
                        let full = self.expand(receiver);
                        self.mention(scope.owner, receiver);
                        self.facts.owner(scope.owner).calls.insert((full, function));
                    } else if receiver == "__MODULE__" {
                        self.facts.owner(scope.owner).refs.insert(function);
                    } else {
                        // `value.function(...)` or `value.field`: whatever
                        // the value is decides at run time.
                        self.node(left, scope, module);
                        self.facts.owner(scope.owner).atoms.insert(function);
                    }
                    for child in children(node) {
                        if Some(child) != target {
                            self.node(child, scope, module);
                        }
                    }
                }
                // An anonymous call or a computed target.
                _ => {
                    for child in children(node) {
                        self.node(child, scope, module);
                    }
                }
            }
            return;
        };

        if scope.module_level
            && scope.owner == Owner::Top
            && module.is_some()
            && self.route(node, scope, module)
        {
            return;
        }

        if MODULE_DEFINERS.contains(&name) && scope.module_level {
            let args = arguments(node);
            let declared = args
                .first()
                .filter(|a| a.kind() == "alias")
                .map(|a| text(*a, self.source).to_string());
            let full = match (&declared, module) {
                (Some(declared), Some(outer)) if name != "defimpl" => {
                    format!("{outer}.{declared}")
                }
                (Some(declared), _) => declared.clone(),
                (None, outer) => outer.unwrap_or("").to_string(),
            };
            let outer_impl = std::mem::take(&mut self.impl_of);
            if name == "defimpl" {
                // Implementing a protocol uses it and the type it is for;
                // their users reach the implementation through dispatch.
                for arg in &args {
                    self.node(*arg, scope, module);
                    self.impl_modules(*arg);
                }
            } else if !full.is_empty() {
                self.facts.modules.push(full.clone());
            }
            let inner = Scope {
                owner: Owner::Top,
                in_impl: name == "defimpl",
                module_level: true,
                pattern: false,
            };
            for child in children(node) {
                if child.kind() == "do_block" {
                    self.block(child, inner, Some(&full));
                }
            }
            self.impl_of = outer_impl;
            return;
        }

        if DEFINERS.contains(&name) && scope.module_level {
            let Some(defined) = arguments(node)
                .first()
                .and_then(|head| self.defined_name(*head))
            else {
                for child in children(node) {
                    self.node(child, scope, module);
                }
                return;
            };
            let kind = if name.starts_with("defmacro") || name.starts_with("defguard") {
                DefKind::Macro
            } else {
                DefKind::Function
            };
            let mut def = Def::new(defined.clone(), kind, span(node));
            def.callback = scope.in_impl || CALLBACKS.contains(&defined.as_str());
            if name == "defdelegate" {
                // Delegates to a function of the same name unless `as:`
                // (an atom, picked up by the walk) says otherwise.
                def.atoms.insert(defined.clone());
            }
            def.via = self.impl_of.clone();
            let owner = self.facts.push(def);
            let inner = Scope {
                owner,
                in_impl: false,
                module_level: false,
                pattern: false,
            };
            for child in children(node) {
                if child.kind() == "arguments" {
                    // The head (parameter patterns and guard), then any
                    // `do:` body given as a keyword.
                    for (i, arg) in children(child).into_iter().enumerate() {
                        let pattern = i == 0 && name != "defdelegate";
                        self.node(arg, Scope { pattern, ..inner }, module);
                    }
                } else if child.kind() != "identifier" {
                    self.node(child, inner, module);
                }
            }
            // Its own name is not a reference to something else.
            self.facts.owner(owner).refs.remove(&defined);
            return;
        }

        if self.test_file && TEST_MACROS.contains(&name) && scope.module_level {
            let title = arguments(node)
                .first()
                .map(|a| text(*a, self.source).trim_matches('"').to_string())
                .unwrap_or_default();
            let owner = self.facts.push(Def::new(title, DefKind::Test, span(node)));
            let inner = Scope {
                owner,
                in_impl: false,
                module_level: false,
                pattern: false,
            };
            for child in children(node) {
                if child.kind() != "identifier" {
                    self.node(child, inner, module);
                }
            }
            return;
        }

        match name {
            // Already recorded; the directive itself is not a use.
            "alias" => return,
            "use" | "import" => {
                // Everything in the file can call what these bring in.
                for arg in arguments(node) {
                    if arg.kind() == "alias" {
                        let full = self.expand(text(arg, self.source));
                        self.facts.uses_all.push(full);
                    }
                }
            }
            "describe" if self.test_file => {
                for child in children(node) {
                    if child.kind() == "do_block" {
                        self.block(child, scope, module);
                    } else if child.kind() != "identifier" {
                        self.node(child, scope, module);
                    }
                }
                return;
            }
            _ => {}
        }
        self.facts.owner(scope.owner).refs.insert(name.to_string());
        for child in children(node) {
            if child.kind() != "identifier" {
                self.node(child, scope, module);
            }
        }
    }

    /// The text of a string or sigil when it reads as a URL path, with each
    /// interpolation replaced by [`WILDCARD`].
    fn path_literal(&self, node: Node) -> Option<String> {
        let mut path = String::new();
        for child in children(node) {
            match child.kind() {
                "quoted_content" => path.push_str(text(child, self.source)),
                "interpolation" => path.push(WILDCARD),
                "escape_sequence" => return None,
                _ => {}
            }
        }
        let plausible = path.starts_with('/')
            && path.len() > 1
            && !path.starts_with("//")
            && !path.contains(char::is_whitespace);
        plausible.then_some(path)
    }

    /// Route declarations in a module body, recognised by shape rather than
    /// by macro name so that routing DSLs built on Phoenix's work too:
    ///
    /// - `scope "/prefix", Alias do … end`: a call whose first argument is a
    ///   path and that has a block nests the routes inside it.
    /// - `get "/path", Controller, :action`: a call whose first argument is
    ///   a path and whose second is a module is a route. It becomes a
    ///   definition that uses the module and serves the path.
    ///
    /// Returns whether `node` was one of these.
    fn route(&mut self, node: Node, scope: Scope, module: Option<&str>) -> bool {
        let args = arguments(node);
        let Some(path) = args
            .first()
            .filter(|a| a.kind() == "string")
            .and_then(|a| {
                self.path_literal(*a)
                    .or_else(|| (text(*a, self.source) == "\"/\"").then(|| "/".to_string()))
            })
            .filter(|p| !p.contains(WILDCARD))
        else {
            return false;
        };
        let target = args.get(1).filter(|a| a.kind() == "alias");
        let block = children(node).into_iter().find(|c| c.kind() == "do_block");
        if let Some(block) = block {
            let alias = target.map(|a| self.expand(text(*a, self.source)));
            self.route_scopes.push((path, alias));
            self.block(block, scope, module);
            self.route_scopes.pop();
            return true;
        }
        let Some(target) = target else {
            return false;
        };
        let mut pattern = String::new();
        for (prefix, _) in &self.route_scopes {
            pattern.push_str(prefix.trim_end_matches('/'));
        }
        pattern.push_str(&path);
        let named = self.expand(text(*target, self.source));
        // Inside `scope "/", Web do`, Phoenix reads `PageController` as
        // `Web.PageController`. Both readings are recorded; the one that is
        // not a module resolves to nothing.
        let prefix: Vec<&str> = self
            .route_scopes
            .iter()
            .filter_map(|(_, alias)| alias.as_deref())
            .collect();
        let mut def = Def::new(format!("route {pattern}"), DefKind::Function, span(node));
        def.uses.insert(named.clone());
        if !prefix.is_empty() {
            def.uses.insert(format!("{}.{named}", prefix.join(".")));
        }
        def.route = Some(pattern);
        let owner = self.facts.push(def);
        let inner = Scope {
            owner,
            in_impl: false,
            module_level: false,
            pattern: false,
        };
        // Remaining arguments: the action atom, options.
        for arg in args.iter().skip(2) {
            self.node(*arg, inner, module);
        }
        true
    }

    /// Record the modules named in a `defimpl` head.
    fn impl_modules(&mut self, node: Node) {
        if node.kind() == "alias" {
            let full = self.expand(text(node, self.source));
            self.impl_of.push(full);
            return;
        }
        for child in children(node) {
            self.impl_modules(child);
        }
    }

    /// The function a `def` head defines: `f(a)`, `f(a) when g` or `f`.
    fn defined_name(&self, head: Node) -> Option<String> {
        match head.kind() {
            "call" => call_name(head, self.source).map(str::to_string),
            "identifier" => Some(text(head, self.source).to_string()),
            "binary_operator" => self.defined_name(head.child_by_field_name("left")?),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::related::facts::LineClass;

    const SOURCE: &str = r#"defmodule Shop.Cart do
  @moduledoc "Carts."
  alias Shop.{Pricing, Stock}
  alias Shop.Repo.Query, as: Q
  use Shop.Schema

  @doc "Total."
  def total(cart) when is_map(cart), do: Pricing.sum(cart.items)

  # reserve stock
  defp reserve(item), do: Stock.hold(item, :fast)

  @impl true
  def handle_call(:total, _from, state), do: {:reply, Q.run(state), state}

  defmacro traced(body), do: body
end
"#;

    #[test]
    fn extracts_functions_with_resolved_module_mentions() {
        let facts = extract(Path::new("lib/shop/cart.ex"), SOURCE);
        assert_eq!(facts.modules, ["Shop.Cart"]);
        assert_eq!(facts.uses_all, ["Shop.Schema"]);
        let total = facts.defs.iter().find(|d| d.name == "total").unwrap();
        assert!(total.uses.contains("Shop.Pricing"));
        assert!(total
            .calls
            .contains(&("Shop.Pricing".to_string(), "sum".to_string())));
        assert!(!total.refs.contains("sum"));
        assert!(!total.callback);
        let reserve = facts.defs.iter().find(|d| d.name == "reserve").unwrap();
        assert!(reserve.uses.contains("Shop.Stock"));
        assert!(reserve.atoms.contains("fast"));
        let callback = facts.defs.iter().find(|d| d.name == "handle_call").unwrap();
        assert!(callback.callback);
        assert!(callback.uses.contains("Shop.Repo.Query"));
        let traced = facts.defs.iter().find(|d| d.name == "traced").unwrap();
        assert_eq!(traced.kind, DefKind::Macro);
    }

    #[test]
    fn type_attributes_are_inert_and_changeset_is_an_ordinary_function() {
        let source = "defmodule Shop.Order do\n  @type t :: %__MODULE__{owner: Shop.User.t()}\n\n  def changeset(order, attrs), do: Shop.Line.cast(order, attrs)\nend\n";
        let facts = extract(Path::new("lib/shop/order.ex"), source);
        assert_eq!(facts.class_of(2), LineClass::Inert);
        assert!(!facts.top.uses.contains("Shop.User"));
        let changeset = facts.defs.iter().find(|d| d.name == "changeset").unwrap();
        assert!(changeset.uses.contains("Shop.Line"));
        assert!(!changeset.callback);
    }

    #[test]
    fn modules_matched_in_a_function_head_are_not_used() {
        let source = "defmodule Shop.Limits do\n  def limit(Shop.Api.List, %Shop.Grant{} = grant), do: Shop.Quota.of(grant)\n  def limit(_other, _grant), do: 0\nend\n";
        let facts = extract(Path::new("lib/shop/limits.ex"), source);
        let limit = &facts.defs[0];
        assert!(limit.uses.contains("Shop.Quota"));
        assert!(!limit.uses.contains("Shop.Api.List"));
        assert!(!limit.uses.contains("Shop.Grant"));
    }

    #[test]
    fn routes_are_definitions_that_serve_a_path() {
        let source = r#"defmodule ShopWeb.Router do
  use ShopWeb, :router

  pipeline :api do
    plug ShopWeb.Auth
  end

  scope "/api/v1", ShopWeb do
    pipe_through :api

    scope "/orgs/:org" do
      get "/members", MemberController, :index
      api_action("/invites", ShopWeb.Api.Invites.Create, tags: ["x"])
    end
  end
end
"#;
        let facts = extract(Path::new("lib/shop_web/router.ex"), source);
        let routes: Vec<&Def> = facts.defs.iter().filter(|d| d.route.is_some()).collect();
        assert_eq!(routes.len(), 2);
        assert_eq!(
            routes[0].route.as_deref(),
            Some("/api/v1/orgs/:org/members")
        );
        assert!(routes[0].uses.contains("ShopWeb.MemberController"));
        assert!(routes[0].atoms.contains("index"));
        assert_eq!(
            routes[1].route.as_deref(),
            Some("/api/v1/orgs/:org/invites")
        );
        assert!(routes[1].uses.contains("ShopWeb.Api.Invites.Create"));
        // Pipelines stay file-level code: a plug serves every route.
        assert!(facts.top.uses.contains("ShopWeb.Auth"));
        assert!(!facts.top.uses.contains("ShopWeb.MemberController"));
    }

    #[test]
    fn path_literals_keep_their_shape_through_interpolation() {
        let source = "defmodule ShopWeb.MemberTest do\n  use ShopWeb.ConnCase\n\n  test \"lists\", %{conn: conn} do\n    get(conn, ~p\"/api/v1/orgs/#{org.id}/members?page=2\")\n    post(conn, \"/api/v1/orgs/#{org.id}\" <> \"/invites\")\n  end\nend\n";
        let facts = extract(Path::new("test/shop_web/member_test.exs"), source);
        let test = facts.defs.iter().find(|d| d.is_test()).unwrap();
        assert_eq!(
            test.paths,
            [
                format!("/api/v1/orgs/{WILDCARD}/members?page=2"),
                format!("/api/v1/orgs/{WILDCARD}"),
                "/invites".to_string()
            ]
        );
        use crate::related::facts::path_reaches;
        assert!(path_reaches(&test.paths[0], "/api/v1/orgs/:org/members"));
        // A base path reaches the routes beneath it.
        assert!(path_reaches(&test.paths[1], "/api/v1/orgs/:org/invites"));
        assert!(!path_reaches(&test.paths[0], "/api/v1/orgs/:org/invites"));
        assert!(!path_reaches(&test.paths[2], "/api/v1/orgs/:org/invites"));
        assert!(!path_reaches("/api/v1/teams", "/api/v1/orgs/:org/members"));
    }

    #[test]
    fn alias_lines_are_imports_of_the_modules_they_name() {
        let facts = extract(Path::new("lib/shop/cart.ex"), SOURCE);
        let LineClass::Import(index) = facts.class_of(3) else {
            panic!("line 3 is {:?}", facts.class_of(3));
        };
        let mut specs = facts.imports[index as usize].specs.clone();
        specs.sort();
        assert_eq!(specs, ["Shop.Pricing", "Shop.Stock"]);
    }

    #[test]
    fn documentation_and_comments_are_inert() {
        let facts = extract(Path::new("lib/shop/cart.ex"), SOURCE);
        assert_eq!(facts.class_of(2), LineClass::Inert);
        assert_eq!(facts.class_of(7), LineClass::Inert);
        assert_eq!(facts.class_of(10), LineClass::Inert);
        assert_eq!(facts.class_of(5), LineClass::Top);
        assert!(matches!(facts.class_of(8), LineClass::Def(_)));
    }

    #[test]
    fn extracts_tests_inside_describe() {
        let source = r#"defmodule Shop.CartTest do
  use ExUnit.Case
  alias Shop.Cart

  setup do
    {:ok, cart: Cart.new()}
  end

  describe "total/1" do
    test "sums items", %{cart: cart} do
      assert Cart.total(cart) == 0
    end
  end
end
"#;
        let facts = extract(Path::new("test/shop/cart_test.exs"), source);
        assert!(facts.is_test_file);
        let test = facts.defs.iter().find(|d| d.is_test()).unwrap();
        assert_eq!(test.name, "sums items");
        assert!(test.uses.contains("Shop.Cart"));
        assert!(test
            .calls
            .contains(&("Shop.Cart".to_string(), "total".to_string())));
        assert!(facts.top.uses.contains("Shop.Cart"));
        assert!(facts
            .top
            .calls
            .contains(&("Shop.Cart".to_string(), "new".to_string())));
    }
}
