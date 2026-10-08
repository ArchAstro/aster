//! Source-level related-test selection for `aster affected --related`.
//!
//! The project graph says which projects a change can reach. This module
//! answers the narrower question of which tests can observe it, by building
//! a dependency graph of the workspace's sources with tree-sitter:
//!
//! 1. Every Elixir, TypeScript/JavaScript, Go, Python and Rust file is parsed into
//!    its definitions (functions, methods, types, constants, tests), the
//!    names each definition mentions, and the files it draws those names
//!    from (imports, module names, package paths).
//! 2. The diff's hunks are mapped onto definitions. A change to comments,
//!    documentation or blank lines selects nothing; a changed import marks
//!    the definitions that use it; any other change outside a definition
//!    counts as a change to the whole file.
//! 3. From each changed definition the walk follows references backwards: a
//!    definition is affected when it mentions an affected name and uses the
//!    file that defines it. Methods are matched by name alone within the
//!    projects downstream of their own, since callers reach them through a
//!    value. A definition nothing names (a framework callback, a macro)
//!    affects every user of its file.
//! 4. The affected tests, grouped by project, are what runs.
//!
//! The analysis is syntactic, so it over-approximates, and where it cannot
//! see it runs more: manifests, lockfiles and tool configuration run their
//! project in full, as do files no source names, changes that reach no
//! test, and projects that load code by computed names. Within the declared
//! project graph, a dependency the source graph cannot explain (another
//! language, a language that is not analysed, no import at all) is honoured
//! at project level, as `--dependents` would, unless the dependent says in
//! `[consumes]` which of its sources use the dependency (see `consumers`).

mod consumers;
mod elixir;
mod engine;
mod facts;
mod go;
mod js;
mod python;
mod resolve;
mod rust;

pub use engine::{analyse, Change, GraphNode, Outcome, Related, TestSelection};
pub use facts::Family;
