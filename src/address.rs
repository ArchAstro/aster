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
                    return Err(anyhow!("Target name cannot be empty after colon: got '{s}'"));
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
        assert_eq!(addr.path, PathBuf::from("src/ts/platform-sdk/examples/nextjs"));
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
        assert!(result.unwrap_err().to_string().contains("must start with //"));
    }

    #[test]
    fn test_display_without_target() {
        let addr = Address {
            path: PathBuf::from("services/api"),
            target: None,
        };
        assert_eq!(format!("{}", addr), "//services/api");
    }

    #[test]
    fn test_display_with_target() {
        let addr = Address {
            path: PathBuf::from("services/api"),
            target: Some("build".to_string()),
        };
        assert_eq!(format!("{}", addr), "//services/api:build");
    }
}
