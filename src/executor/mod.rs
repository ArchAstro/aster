//! Command execution engine
//!
//! Executes target commands on projects in dependency order with parallel
//! execution per DAG level.

pub mod logs;
pub mod runner;

pub use logs::{LogStore, RunLog, TargetLog};
pub use runner::{ExecutionResult, Executor};
