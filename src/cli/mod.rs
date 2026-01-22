//! CLI module for aster commands
//!
//! Provides the command-line interface using clap derive macros.

pub mod commands;
pub mod run;

pub use commands::{Cli, Commands};
pub use run::{expand_selection, parse_run_args, select_projects, RunArgs};
