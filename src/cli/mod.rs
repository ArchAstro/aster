//! CLI module for aster commands
//!
//! Provides the command-line interface using clap derive macros.

pub mod commands;

pub use commands::{Cli, Commands};
