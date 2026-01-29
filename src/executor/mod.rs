//! Command execution engine
//!
//! Executes target commands on projects in dependency order with parallel
//! execution per DAG level.

pub mod logs;
pub mod runner;

pub use logs::{LogStore, RunLog, TargetLog};
pub use runner::{
    collect_target_deps, compute_target_levels, parse_target_address, ExecutionResult, Executor,
};
