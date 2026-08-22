use anyhow::{anyhow, Result};
use std::fmt;
use std::path::PathBuf;

/// A Bazel-style address for referencing projects and targets.
///
/// Format: `//path/to/project:target` or `//path/to/project`
/// Recursive globs: `//path/...` matches all projects under path
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// Path component (relative to workspace root)
    pub path: PathBuf,
    /// Optional target name after the colon
    pub target: Option<String>,
}

impl Address {
    /// Parse an address string into an Address struct.
    ///
    /// Valid formats:
    /// - `//services/api` - path only
    /// - `//services/api:build` - path with target
    /// - `//services/...` - recursive glob
    /// - `//...` - root recursive glob
    pub fn parse(s: &str) -> Result<Self> {
        if !s.starts_with("//") {
            return Err(anyhow!("Address must start with //: got '{s}'"));
        }

        // Remove the // prefix
        let rest = &s[2..];

        // Split on colon to separate path from target
        let (path_str, target) = match rest.find(':') {
            Some(idx) => {
                let path = &rest[..idx];
                let target = &rest[idx + 1..];
                if target.is_empty() {
                    return Err(anyhow!(
                        "Target name cannot be empty after colon: got '{s}'"
                    ));
                }
                (path, Some(target.to_string()))
            }
            None => (rest, None),
        };

        Ok(Address {
            path: PathBuf::from(path_str),
            target,
        })
    }

    /// Returns true if this address is a recursive glob pattern.
    ///
    /// Matches addresses like `//services/...` or `//...`
    pub fn is_recursive(&self) -> bool {
        let path_str = self.path.to_string_lossy();
        path_str.ends_with("/...") || path_str == "..."
    }
}

/// A reusable selector for project addresses.
///
/// Supported forms are exact addresses (`//services/api`), recursive prefixes
/// (`//services/...`), and the workspace-wide selector (`//...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSelector {
    All,
    Recursive(String),
    Exact(String),
}

impl ProjectSelector {
    pub fn parse(selector: &str) -> Result<Self> {
        if selector == "//..." {
            return Ok(Self::All);
        }
        if !selector.starts_with("//") {
            return Err(anyhow!(
                "project selector must start with '//': '{selector}'"
            ));
        }
        if selector.contains(':') {
            return Err(anyhow!(
                "project selector must not include a target: '{selector}'"
            ));
        }
        if let Some(prefix) = selector.strip_suffix("/...") {
            if prefix == "//" || prefix.contains("...") {
                return Err(anyhow!("invalid recursive project selector: '{selector}'"));
            }
            return Ok(Self::Recursive(prefix.to_string()));
        }
        if selector.contains("...") || selector.ends_with('/') {
            return Err(anyhow!("invalid project selector: '{selector}'"));
        }
        Ok(Self::Exact(selector.to_string()))
    }

    pub fn matches(&self, project_address: &str) -> bool {
        // Addresses are slash-delimited on every platform, while paths rendered
        // by `Path::display` use the host separator on Windows.
        let project_address = project_address.replace('\\', "/");
        match self {
            Self::All => true,
            Self::Recursive(prefix) => {
                project_address == *prefix || project_address.starts_with(&format!("{prefix}/"))
            }
            Self::Exact(address) => project_address == *address,
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "//{}", self.path.display())?;
        if let Some(ref target) = self.target {
            write!(f, ":{target}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_path() {
        let addr = Address::parse("//services/api").unwrap();
        assert_eq!(addr.path, PathBuf::from("services/api"));
        assert_eq!(addr.target, None);
    }

    #[test]
    fn test_parse_with_target() {
        let addr = Address::parse("//services/api:build").unwrap();
        assert_eq!(addr.path, PathBuf::from("services/api"));
        assert_eq!(addr.target, Some("build".to_string()));
    }

    #[test]
    fn test_parse_nested_path() {
        let addr = Address::parse("//src/ts/platform-sdk/examples/nextjs:dev").unwrap();
        assert_eq!(
            addr.path,
            PathBuf::from("src/ts/platform-sdk/examples/nextjs")
        );
        assert_eq!(addr.target, Some("dev".to_string()));
    }

    #[test]
    fn test_parse_recursive_glob() {
        let addr = Address::parse("//services/...").unwrap();
        assert_eq!(addr.path, PathBuf::from("services/..."));
        assert!(addr.is_recursive());
    }

    #[test]
    fn test_parse_root_glob() {
        let addr = Address::parse("//...").unwrap();
        assert_eq!(addr.path, PathBuf::from("..."));
        assert!(addr.is_recursive());
    }

    #[test]
    fn test_parse_invalid_no_prefix() {
        let result = Address::parse("services/api");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must start with //"));
    }

    #[test]
    fn project_selectors_match_exact_prefix_and_all() {
        assert!(ProjectSelector::parse("//services/api")
            .unwrap()
            .matches("//services/api"));
        assert!(!ProjectSelector::parse("//services/api")
            .unwrap()
            .matches("//services/api/web"));
        assert!(ProjectSelector::parse("//services/...")
            .unwrap()
            .matches("//services/api/web"));
        assert!(ProjectSelector::parse("//...")
            .unwrap()
            .matches("//services/api"));
        assert!(ProjectSelector::parse("//services/...")
            .unwrap()
            .matches("//services\\api"));
    }

    #[test]
    fn project_selectors_reject_paths_targets_and_malformed_recursion() {
        for selector in [
            "services/api",
            "//services/api:test",
            "//services/.../api",
            "//services/",
        ] {
            assert!(ProjectSelector::parse(selector).is_err(), "{selector}");
        }
    }

    #[test]
    fn test_display_without_target() {
        let addr = Address {
            path: PathBuf::from("services/api"),
            target: None,
        };
        assert_eq!(format!("{addr}"), "//services/api");
    }

    #[test]
    fn test_display_with_target() {
        let addr = Address {
            path: PathBuf::from("services/api"),
            target: Some("build".to_string()),
        };
        assert_eq!(format!("{addr}"), "//services/api:build");
    }
}
