//! CLI module for aster commands
//!
//! Provides the command-line interface using clap derive macros.

pub mod commands;
pub mod output;
pub mod run;

pub use commands::{Cli, Commands};
pub use output::{
    build_execution_output, output_json, print_summary, ExecutionOutput, ExecutionSummary,
    GraphOutput, OutputMode, ProjectInfo, TargetResult, WhyOutput,
};
pub use run::{check_reserved_target, expand_selection, parse_run_args, select_projects, RunArgs};
