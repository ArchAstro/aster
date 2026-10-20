//! CLI module for aster commands
//!
//! Provides the command-line interface using clap derive macros.

pub mod affected_files;
pub mod cache_plan;
pub mod commands;
pub mod output;
pub mod run;
mod skills;
pub mod source_graph;

pub use commands::{
    CacheCommands, CiCommands, Cli, Commands, ProjectCommands, ResultsCommands, ServicesCommands,
    ServicesDaemonCommands, TlsCommands,
};
pub use output::{
    build_execution_output, build_execution_output_with_cache, output_json, print_summary,
    ExecutionOutput, ExecutionSummary, GraphOutput, OutputMode, ProjectInfo, TargetResult,
    WhyOutput,
};
pub use run::{check_reserved_target, expand_selection, parse_run_args, select_projects, RunArgs};
pub use skills::SKILLS_MARKDOWN;
