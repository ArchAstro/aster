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
    /// Some call in the file picks a function by a computed name.
    applies: bool,
    /// Some call in the file looks inside a module (see [`inspects_module`]).
    inspects: bool,
    /// Some call in the file loads modules (see [`loads_module`]).
    loads: bool,
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
        applies: false,
        inspects: false,
        loads: false,
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
    if walker.loads && !walker.inspects {
        // Loading what it lists is all the file does with it, and a test
        // cannot tell a module that was loaded early from one loaded on use.
        for def in facts.defs.iter_mut().chain([&mut facts.top]) {
            if def.reflects == Some(LISTS_MODULES) {
                def.reflects = None;
            }
        }
    }
    if walker.applies {
        for def in facts.defs.iter_mut().chain([&mut facts.top]) {
            if def.reflects == Some(LISTS_MODULES) {
                def.reflects = Some(LISTS_AND_APPLIES);
            }
        }
    }
    facts.classify_lines(source, &walker.inert);
    super::annotations::apply(&mut facts, &tree, source);
    facts
}

/// Why a definition that lists modules is affected by a change to the
/// module's shape (what it exports, its attributes, its macros) but not by
/// an edit inside a function body.
pub const LISTS_MODULES: &str = "lists modules at run time";
/// A definition that lists modules and also calls what it finds by a name
/// computed at run time (`apply(module, name, args)`) or reads what modules
/// export (`module.__info__(:functions)`). Nothing in its source names the
/// functions it reaches, so every source change in the project reaches it.
pub const LISTS_AND_APPLIES: &str =
    "lists modules at run time and calls what it finds by a computed name";

/// Modules whose functions never look inside a module they are given.
const OPAQUE_MODULES: &[&str] = &[
    "Application",
    "Code",
    "Enum",
    "Kernel",
    "Keyword",
    "List",
    "Map",
    "MapSet",
    "Path",
    "Stream",
    ":application",
    ":code",
    ":lists",
    ":maps",
    "__MODULE__",
];
/// Functions that look inside a module: what it exports, or how it is built.
const INSPECTORS: &[&str] = &[
    "function_exported?",
    "macro_exported?",
    "module_info",
    "__info__",
    "behaviour_info",
    "apply",
];

/// Whether a call looks inside a module, or hands one to code this file
/// cannot see into. Calling a function on a value does, and so does calling
/// any module outside [`OPAQUE_MODULES`].
fn inspects_module(receiver: Option<&str>, function: &str) -> bool {
    INSPECTORS.contains(&function)
        || receiver.is_some_and(|module| !OPAQUE_MODULES.contains(&module))
}

/// Whether a call loads a module without looking at it.
fn loads_module(receiver: &str, function: &str) -> bool {
    receiver == "Code"
        && matches!(
            function,
            "ensure_loaded" | "ensure_all_loaded" | "ensure_loaded!" | "ensure_all_loaded!"
        )
}

/// Whether a call picks a function of another module by a name that is not
/// written in the source, or reads the functions a module exports.
fn applies_by_computed_name(
    receiver: Option<&str>,
    function: &str,
    call: Node,
    source: &str,
) -> bool {
    let args = arguments(call);
    match (receiver, function) {
        (None | Some(":erlang" | "Kernel"), "apply") => args.len() == 3 && args[1].kind() != "atom",
        (_, "__info__") => args
            .first()
            .is_none_or(|a| !matches!(text(*a, source), ":module" | ":attributes" | ":compile")),
        (_, "module_info") => args
            .first()
            .is_none_or(|a| !matches!(text(*a, source), ":module" | ":attributes" | ":compile")),
        _ => false,
    }
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

/// The tag of a message written as a literal atom or string, or as a tuple
/// led by one: `a:name` or `s:text`. `None` when the head is computed.
fn literal_tag(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "atom" | "quoted_atom" => Some(format!(
            "a:{}",
            text(node, source).trim_start_matches(':').trim_matches('"')
        )),
        "string" => {
            let parts = children(node);
            let plain = parts
                .iter()
                .all(|p| p.kind() == "quoted_content" || p.kind() == "\"");
            let content: String = parts
                .iter()
                .filter(|p| p.kind() == "quoted_content")
                .map(|p| text(*p, source))
                .collect();
            (plain && !content.contains('\\')).then(|| format!("s:{content}"))
        }
        "tuple" => {
            let first = children(node).into_iter().find(|c| c.is_named())?;
            matches!(first.kind(), "atom" | "quoted_atom" | "string")
                .then(|| literal_tag(first, source))
                .flatten()
        }
        _ => None,
    }
}

/// The tag a message pattern requires, looking through `pattern = name`
/// and `name = pattern`. `None` when it can match a message of any head.
fn pattern_tag(node: Node, source: &str) -> Option<String> {
    if node.kind() == "binary_operator"
        && node
            .child_by_field_name("operator")
            .is_some_and(|o| text(o, source) == "=")
    {
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        return pattern_tag(left, source).or_else(|| pattern_tag(right, source));
    }
    literal_tag(node, source)
}

/// Whether a call receives its first argument from a `|>`.
fn piped(call: Node, source: &str) -> bool {
    let Some(parent) = call.parent() else {
        return false;
    };
    parent.kind() == "binary_operator"
        && parent.child_by_field_name("right") == Some(call)
        && parent
            .child_by_field_name("operator")
            .is_some_and(|op| text(op, source) == "|>")
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
                    let named = text(node, self.source);
                    self.mention(scope.owner, named);
                    let full = self.expand(named);
                    self.facts.owner(scope.owner).bare.insert(full);
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
                let line = node.start_position().row + 1;
                self.facts.owner(scope.owner).string(&content, line);
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
                if node.kind() == "tuple" {
                    self.implicit_message(node, scope.owner);
                }
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
                    let receiver_module = self.expand(receiver);
                    self.send_site(scope.owner, Some(&receiver_module), &function, node);
                    self.applies |=
                        applies_by_computed_name(Some(receiver), &function, node, self.source);
                    self.inspects |= inspects_module(Some(receiver), &function);
                    self.loads |= loads_module(receiver, &function);
                    if lists_modules(receiver, &function, node, self.source) {
                        self.facts.owner(scope.owner).reflects = Some(LISTS_MODULES);
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
            if name == "def" {
                if let Some(head) = arguments(node).first() {
                    def.accepts = self.accepted_tag(&defined, *head);
                }
            }
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

        self.applies |= applies_by_computed_name(None, name, node, self.source);
        self.inspects |= inspects_module(None, name);
        match name {
            // Already recorded; the directive itself is not a use.
            "alias" => return,
            "use" | "import" => {
                // Everything in the file can call what these bring in.
                for arg in arguments(node) {
                    self.inspects |= !OPAQUE_MODULES.contains(&text(arg, self.source));
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
        self.send_site(scope.owner, None, name, node);
        self.facts.owner(scope.owner).refs.insert(name.to_string());
        for child in children(node) {
            if child.kind() != "identifier" {
                self.node(child, scope, module);
            }
        }
    }

    /// Record the message a call sends, when it is one that delivers a
    /// message to a process callback. `module` is the expanded receiver of
    /// a remote call, `None` for a bare call.
    fn send_site(&mut self, owner: Owner, module: Option<&str>, function: &str, call: Node) {
        let args = arguments(call);
        // `pid |> GenServer.call(msg)` leaves the first argument out.
        let skipped = usize::from(piped(call, self.source));
        let message = |index: usize| args.get(index.wrapping_sub(skipped)).copied();
        let index = match (module, function) {
            (
                Some("GenServer" | ":gen_server" | ":gen_statem" | "GenStateMachine"),
                "call" | "cast" | "send_request",
            ) => 1,
            (Some("Process" | ":erlang" | "Kernel"), "send") | (None, "send") => 1,
            (Some("Process"), "send_after") => 1,
            (Some(":erlang"), "send_after" | "start_timer") => 2,
            (Some(":timer"), "send_after" | "send_interval") => {
                if args.len() + skipped == 2 {
                    1
                } else {
                    2
                }
            }
            (Some("Phoenix.PubSub"), "broadcast" | "broadcast!" | "local_broadcast") => 2,
            (Some("Phoenix.PubSub"), "broadcast_from" | "broadcast_from!") => 3,
            // Phoenix.ChannelTest's `push(socket, "event", payload)`.
            (None, "push") => 1,
            // Delivery to every subscriber, or to nodes: anything may go.
            (_, "broadcast" | "broadcast!" | "broadcast_from" | "broadcast_from!")
            | (_, "local_broadcast" | "direct_broadcast" | "abcast" | "multi_call")
            | (Some("Registry"), "dispatch") => {
                self.facts.owner(owner).sends_any = true;
                return;
            }
            _ => return,
        };
        let tag = message(index).and_then(|node| literal_tag(node, self.source));
        let def = self.facts.owner(owner);
        match tag {
            Some(tag) => {
                def.sends.insert(tag);
            }
            None => def.sends_any = true,
        }
    }

    /// Return values that deliver a message to the process itself:
    /// `{:continue, term}` and a state machine's `{:next_event, type,
    /// content}` and event timeouts.
    fn implicit_message(&mut self, tuple: Node, owner: Owner) {
        let items: Vec<Node> = children(tuple)
            .into_iter()
            .filter(|c| c.is_named())
            .collect();
        let Some(head) = items.first().filter(|h| h.kind() == "atom") else {
            return;
        };
        let wanted = match text(*head, self.source) {
            ":continue" => 2,
            ":next_event" | ":state_timeout" | ":event_timeout" => 3,
            ":generic_timeout" => 4,
            _ => return,
        };
        if items.len() != wanted {
            return;
        }
        let tag = items.last().and_then(|n| literal_tag(*n, self.source));
        let def = self.facts.owner(owner);
        match tag {
            Some(tag) => {
                def.sends.insert(tag);
            }
            None => def.sends_any = true,
        }
    }

    /// The tag a message callback clause accepts, from its head: the first
    /// message pattern of `handle_call(pattern, from, state)`.
    fn accepted_tag(&self, name: &str, head: Node) -> Option<String> {
        let mut head = head;
        while head.kind() == "binary_operator" {
            head = head.child_by_field_name("left")?;
        }
        let args = arguments(head);
        let index = match (name, args.len()) {
            ("handle_call" | "handle_in", 3) | ("handle_cast" | "handle_info", 2) => 0,
            ("handle_continue", 2) => 0,
            // A state machine's `handle_event(type, content, state, data)`;
            // a LiveView's takes three.
            ("handle_event", 4) => 1,
            _ => return None,
        };
        let tag = pattern_tag(*args.get(index)?, self.source)?;
        // Messages the runtime sends, not any process.
        const SYSTEM: &[&str] = &[
            "a:DOWN",
            "a:EXIT",
            "a:UP",
            "a:timeout",
            "a:nodeup",
            "a:nodedown",
            "a:tcp",
            "a:tcp_closed",
            "a:tcp_error",
            "a:tcp_passive",
            "a:udp",
            "a:udp_passive",
            "a:ssl",
            "a:ssl_closed",
            "a:ssl_error",
            "a:ssl_passive",
            "a:inet_async",
            "a:inet_reply",
            "a:gun_up",
            "a:gun_down",
            "a:gun_data",
            "a:gun_response",
            "a:gun_ws",
            "a:gun_error",
            "a:gun_upgrade",
            "a:gun_inform",
            "a:gun_push",
            "a:gun_trailers",
            "a:gun_tunnel_up",
            "a:gun_tunnel_down",
            "a:system",
            "a:internal",
            "a:cast",
            "a:info",
            "a:call",
        ];
        (!SYSTEM.contains(&tag.as_str())).then_some(tag)
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

    fn def_named<'a>(facts: &'a FileFacts, name: &str, nth: usize) -> &'a Def {
        facts
            .defs
            .iter()
            .filter(|d| d.name == name)
            .nth(nth)
            .unwrap()
    }

    #[test]
    fn message_clauses_record_the_tag_they_accept() {
        let source = r#"defmodule Shop.Room do
  use GenServer
  def handle_call({:join, user}, _from, state) when is_map(user), do: {:reply, :ok, state}
  def handle_call(:leave, _from, state), do: {:reply, :ok, state}
  def handle_call({:tag, _} = msg, _from, state), do: {:reply, msg, state}
  def handle_call(msg, _from, state), do: {:reply, msg, state}
  def handle_call(%{kind: :map}, _from, state), do: {:reply, :ok, state}
  def handle_call(@attr, _from, state), do: {:reply, :ok, state}
  def handle_cast({tag, _}, state), do: {:noreply, state}
  def handle_info({:DOWN, _, _, _, _}, state), do: {:noreply, state}
  def handle_info(:tick, state), do: {:noreply, state}
  def handle_continue({:load, id}, state), do: {:noreply, state}
  def handle_in("new_msg", payload, socket), do: {:noreply, socket}
  def handle_in("room:" <> id, payload, socket), do: {:noreply, socket}
  def handle_event(:cast, {:poke, n}, state, data), do: {:keep_state, data}
  def handle_event("click", params, socket), do: {:noreply, socket}
end
"#;
        let facts = extract(Path::new("lib/shop/room.ex"), source);
        let tags: Vec<Option<&str>> = facts.defs.iter().map(|d| d.accepts.as_deref()).collect();
        assert_eq!(
            tags,
            [
                Some("a:join"),
                Some("a:leave"),
                Some("a:tag"),
                None,
                None,
                None,
                None,
                // The runtime sends :DOWN; no sender names it.
                None,
                Some("a:tick"),
                Some("a:load"),
                Some("s:new_msg"),
                None,
                Some("a:poke"),
                None,
            ]
        );
    }

    #[test]
    fn send_sites_record_literal_tags_and_unknown_messages() {
        let source = r#"defmodule Shop.Client do
  alias Phoenix.PubSub

  def literal(pid), do: GenServer.call(pid, {:join, 1})
  def atom(pid), do: GenServer.cast(pid, :leave)
  def statem(pid), do: :gen_statem.cast(pid, {:poke, 1})
  def timer, do: Process.send_after(self(), :tick, 10)
  def kernel(pid), do: send(pid, {:ping, self()})
  def piped(pid), do: pid |> GenServer.call({:piped, 1})
  def pubsub, do: PubSub.broadcast(Shop.PubSub, "t", {:event, 1})
  def computed(pid, msg), do: GenServer.call(pid, msg)
  def built(pid, id), do: GenServer.call(pid, {id, 1})
  def fanout(topic), do: ShopWeb.Endpoint.broadcast(topic, "ev", %{})
  def push_event(socket), do: push(socket, "new_msg", %{})
  def interpolated(socket, n), do: push(socket, "ev#{n}", %{})
  def next(state), do: {:noreply, state, {:continue, :load}}
  def reply(from), do: GenServer.reply(from, :ok)
  def unrelated(list), do: Enum.map(list, &(&1 + 1))
end
"#;
        let facts = extract(Path::new("lib/shop/client.ex"), source);
        let sent = |name: &str| {
            let def = def_named(&facts, name, 0);
            (
                def.sends.iter().map(String::as_str).collect::<Vec<_>>(),
                def.sends_any,
            )
        };
        assert_eq!(sent("literal"), (vec!["a:join"], false));
        assert_eq!(sent("atom"), (vec!["a:leave"], false));
        assert_eq!(sent("statem"), (vec!["a:poke"], false));
        assert_eq!(sent("timer"), (vec!["a:tick"], false));
        assert_eq!(sent("kernel"), (vec!["a:ping"], false));
        assert_eq!(sent("piped"), (vec!["a:piped"], false));
        assert_eq!(sent("pubsub"), (vec!["a:event"], false));
        assert_eq!(sent("computed"), (vec![], true));
        assert_eq!(sent("built"), (vec![], true));
        assert_eq!(sent("fanout"), (vec![], true));
        assert_eq!(sent("push_event"), (vec!["s:new_msg"], false));
        assert_eq!(sent("interpolated"), (vec![], true));
        assert_eq!(sent("next"), (vec!["a:load"], false));
        assert_eq!(sent("reply"), (vec![], false));
        assert_eq!(sent("unrelated"), (vec![], false));
    }

    #[test]
    fn modules_are_bare_unless_they_only_receive_a_call() {
        let source = "defmodule Shop.User do\n  alias Shop.{Room, Pool}\n  def a(), do: Room.join(1)\n  def b(), do: {Room.join(1), %Pool{}}\n  def c(pid), do: GenServer.call(Room, pid)\nend\n";
        let facts = extract(Path::new("lib/shop/user.ex"), source);
        assert!(def_named(&facts, "a", 0).bare.is_empty());
        let bare = |name: &str| {
            def_named(&facts, name, 0)
                .bare
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(bare("b"), ["Shop.Pool"]);
        assert_eq!(bare("c"), ["Shop.Room"]);
    }

    fn reflection_of(source: &str) -> Vec<Option<&'static str>> {
        let facts = extract(Path::new("lib/shop/registry.ex"), source);
        facts.defs.iter().map(|d| d.reflects).collect()
    }

    #[test]
    fn listing_modules_is_told_apart_by_what_the_file_does_with_them() {
        let list = "defmodule Shop.Registry do\n  def modules do\n    Application.spec(:shop, :modules)\n  end\n";
        // Probing exports is the plain reason.
        let probe = format!("{list}  def audit, do: Enum.filter(modules(), &function_exported?(&1, :tax, 1))\nend\n");
        assert_eq!(reflection_of(&probe)[0], Some(LISTS_MODULES));
        // Applying a function picked at run time is the broad one.
        let apply =
            format!("{list}  def run(name), do: Enum.each(modules(), &apply(&1, name, []))\nend\n");
        assert_eq!(reflection_of(&apply)[0], Some(LISTS_AND_APPLIES));
        let info =
            format!("{list}  def fns, do: Enum.map(modules(), & &1.__info__(:functions))\nend\n");
        assert_eq!(reflection_of(&info)[0], Some(LISTS_AND_APPLIES));
        // `apply` with the name written out is an atom the index follows.
        let named =
            format!("{list}  def run, do: Enum.each(modules(), &apply(&1, :tax, [1]))\nend\n");
        assert_eq!(reflection_of(&named)[0], Some(LISTS_MODULES));
    }

    #[test]
    fn a_file_that_only_loads_what_it_lists_does_not_reflect() {
        let loads = "defmodule Shop.Preload do\n  def load, do: Application.spec(:shop, :modules) |> Code.ensure_all_loaded()\nend\n";
        assert_eq!(reflection_of(loads), [None]);
        // Anything else done with the modules keeps it.
        let looks = "defmodule Shop.Preload do\n  def load do\n    mods = Application.spec(:shop, :modules)\n    Code.ensure_all_loaded(mods)\n    Enum.filter(mods, &Shop.Check.worker?/1)\n  end\nend\n";
        assert_eq!(reflection_of(looks), [Some(LISTS_MODULES)]);
    }
}
