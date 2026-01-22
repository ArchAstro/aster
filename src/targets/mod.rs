//! Target resolution for discovered projects
//!
//! Provides default commands per language and merges with aster.toml overrides.

pub mod resolver;

pub use resolver::TargetResolver;
