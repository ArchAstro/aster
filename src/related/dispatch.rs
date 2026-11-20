//! Dispatch rules: how a framework reaches a callback that nothing calls by
//! name.
//!
//! Without a rule, a callback (an Oban worker's `perform`, a GenServer's
//! `init`) is reached by every definition that uses its module, because the
//! analysis cannot see the framework call it. Most of those definitions only
//! name the module: they enqueue a job, build a child specification, match
//! on a struct. A rule says which calls actually run the callback:
//!
//! - `callers` run the callback of the module they are handed
//!   (`perform_job(MyWorker, args)`). A definition reaches the callback when
//!   it makes one of these calls and uses the module.
//! - `runners` run the callback of whatever module is waiting for them
//!   (`Oban.drain_queue(queue: :default)`). A definition that makes one of
//!   these calls reaches every callback the rule covers, in its own project
//!   and the projects downstream.
//!
//! A call to the callback by its own name (`MyWorker.perform(job)`) reaches
//! it as any named call does. Rules come from `[affected.related]`: written
//! out as `dispatch` tables, or named as built-in `packs`. A pack is a TOML
//! file under `packs/` in the same keys as `[affected.related]`, so aster's
//! code holds no framework's names.

use super::facts::Def;
use serde::Deserialize;

/// One rule; see the module documentation.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DispatchRule {
    /// What the callback's module takes on: an Elixir module it `use`s or
    /// names in `@behaviour`, or a Rust trait it implements.
    pub behaviour: String,
    /// Names of the callbacks the rule covers.
    pub callbacks: Vec<String>,
    /// Calls that run the callback of a module they are handed. A bare name
    /// (`perform_job`) matches the function however it is qualified; a
    /// qualified one (`Oban.Testing.perform_job`) matches only that module.
    #[serde(default)]
    pub callers: Vec<String>,
    /// Calls that run the callback of any module the rule covers, named as
    /// in [`Self::callers`].
    #[serde(default)]
    pub runners: Vec<String>,
}

/// Files where the walk from a change stops: `[[affected.related.boundary]]`.
///
/// A boundary is a hub (a registry, an allowlist, a process most code goes
/// through) whose callers reach most of the workspace, so following it
/// selects nearly every test. Past a boundary the walk marks only test code
/// (test files and test support): the tests that use the hub directly. The
/// hub's own file is walked as usual.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    /// Workspace-relative globs of the files.
    pub files: Vec<String>,
    /// Why stopping there is safe: which tests still cover the hub.
    pub reason: String,
}

impl Boundary {
    /// Check what config gives.
    pub fn validate(&self) -> Result<(), String> {
        if self.files.is_empty() {
            return Err("a boundary needs `files`".to_string());
        }
        if self.reason.trim().is_empty() {
            return Err(format!(
                "the boundary for {:?} needs a `reason`",
                self.files
            ));
        }
        for glob in &self.files {
            globset::Glob::new(glob).map_err(|e| format!("boundary glob {glob:?}: {e}"))?;
        }
        Ok(())
    }
}

/// Paths that files read without their code showing it:
/// `[[affected.related.reads]]`.
///
/// Each file that `files` matches is treated as if its top-level code
/// carried `aster: reads <paths>`: a changed file that a path covers selects
/// it. This is for tests that reach code only at run time, through a process
/// or a registry that a boundary cuts or that no call names, where one rule
/// for a directory of tests replaces a comment in every file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeclaredReads {
    /// Workspace-relative globs of the files that do the reading.
    pub files: Vec<String>,
    /// Workspace-relative paths, directories or globs they read, as after
    /// `aster: reads`.
    pub paths: Vec<String>,
    /// Why no reference shows the read.
    pub reason: String,
}

impl DeclaredReads {
    /// Check what config gives.
    pub fn validate(&self) -> Result<(), String> {
        if self.files.is_empty() {
            return Err("a reads rule needs `files`".to_string());
        }
        if self.paths.is_empty() {
            return Err(format!("the reads rule for {:?} needs `paths`", self.files));
        }
        if self.reason.trim().is_empty() {
            return Err(format!(
                "the reads rule for {:?} needs a `reason`",
                self.files
            ));
        }
        for glob in &self.files {
            globset::Glob::new(glob).map_err(|e| format!("reads glob {glob:?}: {e}"))?;
        }
        for path in &self.paths {
            let Some(path) = super::annotations::normalise(path) else {
                return Err(format!(
                    "the reads rule for {:?} has a path that names nothing: {path:?}",
                    self.files
                ));
            };
            globset::Glob::new(&path).map_err(|e| format!("reads path {path:?}: {e}"))?;
        }
        Ok(())
    }
}

/// Everything `[affected.related]` says about how the walk goes.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub dispatch: Vec<DispatchRule>,
    pub boundaries: Vec<Boundary>,
    /// Reads declared for whole files.
    pub reads: Vec<DeclaredReads>,
    /// Modules whose `use` calls no function of the module that uses it.
    pub inert_uses: Vec<String>,
    /// Module attributes a library reads at compile time (without `@`).
    pub compile_attributes: Vec<String>,
    /// Where the rules above came from: those written in config, then each
    /// named pack. `aster check` reports a written entry that matches
    /// nothing, but a pack only when none of it matches: a repo uses part
    /// of a framework.
    pub origins: Vec<(Option<String>, Pack)>,
}

impl Rules {
    /// The rules of `written` and of each pack, in that order.
    pub fn new(boundaries: Vec<Boundary>, written: Pack, packs: Vec<(String, Pack)>) -> Self {
        let mut rules = Rules {
            boundaries,
            ..Default::default()
        };
        let origins: Vec<(Option<String>, Pack)> = std::iter::once((None, written))
            .chain(packs.into_iter().map(|(name, pack)| (Some(name), pack)))
            .collect();
        for (_, pack) in &origins {
            rules.dispatch.extend(pack.dispatch.iter().cloned());
            rules.inert_uses.extend(pack.inert_uses.iter().cloned());
            rules
                .compile_attributes
                .extend(pack.compile_attributes.iter().cloned());
        }
        rules.origins = origins;
        rules
    }
}

/// A built-in set of rules, written in the same keys as
/// `[affected.related]`. Each lives in `packs/<name>.toml`: the framework
/// knowledge is data, and adding a pack needs no code.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    #[serde(default)]
    pub dispatch: Vec<DispatchRule>,
    #[serde(default)]
    pub inert_uses: Vec<String>,
    #[serde(default)]
    pub compile_attributes: Vec<String>,
}

/// The built-in packs, by name, as written.
const PACK_SOURCES: &[(&str, &str)] = &[
    ("ecto", include_str!("packs/ecto.toml")),
    ("oban", include_str!("packs/oban.toml")),
    ("protobuf", include_str!("packs/protobuf.toml")),
];

/// Names of the built-in packs `[affected.related] packs` accepts.
pub fn pack_names() -> Vec<&'static str> {
    PACK_SOURCES.iter().map(|(name, _)| *name).collect()
}

/// The built-in pack `name`.
pub fn pack(name: &str) -> Option<Pack> {
    let (_, source) = PACK_SOURCES.iter().find(|(n, _)| *n == name)?;
    // The tests parse and validate every pack, so this cannot fail in a
    // released binary.
    Some(toml::from_str(source).expect("built-in pack parses"))
}

impl Pack {
    /// Check what config or a pack file gives.
    pub fn validate(&self) -> Result<(), String> {
        for rule in &self.dispatch {
            rule.validate()?;
        }
        if let Some(blank) = self
            .inert_uses
            .iter()
            .chain(&self.compile_attributes)
            .find(|name| name.trim().is_empty())
        {
            return Err(format!("{blank:?} is not a module or attribute name"));
        }
        if let Some(at) = self.compile_attributes.iter().find(|a| a.starts_with('@')) {
            return Err(format!(
                "compile attribute {at:?}: write the name without `@`"
            ));
        }
        Ok(())
    }
}

impl DispatchRule {
    /// Whether the rule covers `def`, a callback nothing calls by name.
    pub fn covers(&self, def: &Def) -> bool {
        self.callbacks.contains(&def.name)
            && (def.behaviours.contains(&self.behaviour)
                || def.implements.as_deref() == Some(self.behaviour.as_str()))
    }

    /// Whether `def` makes one of the `calls`.
    pub fn makes(calls: &[String], def: &Def) -> bool {
        calls.iter().any(|call| match call.rsplit_once('.') {
            Some((module, function)) => def.calls.iter().any(|(m, f)| m == module && f == function),
            None => def.refs.contains(call) || def.calls.iter().any(|(_, f)| f == call),
        })
    }

    /// Check what config gives.
    pub fn validate(&self) -> Result<(), String> {
        if self.behaviour.trim().is_empty() {
            return Err("a dispatch rule needs a `behaviour`".to_string());
        }
        if self.callbacks.is_empty() {
            return Err(format!(
                "the dispatch rule for {:?} names no `callbacks`",
                self.behaviour
            ));
        }
        if self.callers.is_empty() && self.runners.is_empty() {
            return Err(format!(
                "the dispatch rule for {:?} names no `callers` or `runners`: nothing would reach its callbacks",
                self.behaviour
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::related::facts::DefKind;

    fn perform(behaviours: &[&str]) -> Def {
        let mut def = Def::new("perform", DefKind::Function, (1, 1));
        def.behaviours = behaviours.iter().map(|b| b.to_string()).collect();
        def
    }

    #[test]
    fn a_rule_covers_its_callbacks_of_modules_that_take_on_its_behaviour() {
        let rule = &pack("oban").unwrap().dispatch[0];
        assert!(rule.covers(&perform(&["Oban.Worker"])));
        assert!(!rule.covers(&perform(&["GenServer"])));
        let mut other = perform(&["Oban.Worker"]);
        other.name = "enqueue".to_string();
        assert!(!rule.covers(&other));
    }

    #[test]
    fn a_bare_call_name_matches_any_qualifier_and_a_qualified_one_only_its_module() {
        let mut def = Def::new("test", DefKind::Test, (1, 1));
        def.calls
            .insert(("Oban".to_string(), "drain_queue".to_string()));
        assert!(DispatchRule::makes(&["drain_queue".to_string()], &def));
        assert!(DispatchRule::makes(&["Oban.drain_queue".to_string()], &def));
        assert!(!DispatchRule::makes(
            &["Other.drain_queue".to_string()],
            &def
        ));
        def.refs.insert("perform_job".to_string());
        assert!(DispatchRule::makes(&["perform_job".to_string()], &def));
    }

    #[test]
    fn a_rule_that_nothing_reaches_is_rejected() {
        let rule = DispatchRule {
            behaviour: "Oban.Worker".to_string(),
            callbacks: vec!["perform".to_string()],
            callers: Vec::new(),
            runners: Vec::new(),
        };
        assert!(rule.validate().is_err());
        assert!(pack("sidekiq").is_none());
    }

    #[test]
    fn every_built_in_pack_parses_and_validates() {
        for name in pack_names() {
            let pack = pack(name).unwrap();
            assert_eq!(pack.validate(), Ok(()), "pack {name}");
            assert_ne!(pack, Pack::default(), "pack {name} is empty");
        }
    }

    #[test]
    fn a_compile_attribute_is_named_without_its_at_sign() {
        let pack = Pack {
            compile_attributes: vec!["@primary_key".to_string()],
            ..Pack::default()
        };
        assert!(pack.validate().is_err());
    }
}
