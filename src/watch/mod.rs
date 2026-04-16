//! `aster watch` — rerun targets when their declared inputs change.
//!
//! The watcher:
//!  - resolves `W = R ∪ transitive_deps(R)` on the target graph,
//!  - builds a [`crate::cache::TargetInputMatcher`] per target in `W`,
//!  - watches every project directory in `W` via `notify`,
//!  - on each fs event, finds the owning target(s) whose cache inputs match
//!    the changed path and expands to the transitive dependents within `W`,
//!  - reruns that primary set through the normal executor pipeline,
//!  - manages `stream = true` targets as long-lived supervised children.

pub mod event_loop;
pub mod ignore;
pub mod plan;
pub mod stream;

pub use event_loop::{run_watch, WatchOpts};
pub use ignore::{WorkspaceIgnore, DEFAULT_IGNORE_GLOBS};
pub use plan::{WatchPlan, WatchTarget};
pub use stream::StreamSupervisor;
