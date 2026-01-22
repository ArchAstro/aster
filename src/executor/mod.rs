//! Command execution engine
//!
//! Executes target commands on projects in dependency order with parallel
//! execution per DAG level.

pub mod runner;

pub use runner::{ExecutionResult, Executor};
