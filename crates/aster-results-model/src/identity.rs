//! Stable test identity.

use sha2::{Digest, Sha256};

/// What names a test across runs: the project that owns it, the file it is
/// in, the suite (module, class, describe block) and its name. Any part a
/// runner does not report is empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TestIdentity {
    /// Aster project address, e.g. `//services/platform`.
    pub project: String,
    /// Path relative to the project directory, with `/` separators.
    pub file: String,
    pub suite: String,
    pub name: String,
}

impl TestIdentity {
    pub fn new(
        project: impl Into<String>,
        file: impl Into<String>,
        suite: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            project: project.into(),
            file: file.into().replace('\\', "/"),
            suite: suite.into(),
            name: name.into(),
        }
    }

    /// Hex SHA-256 of the four parts joined by U+001F (unit separator), a
    /// character that cannot appear in a path or a sane test name, so no two
    /// identities share a key.
    pub fn test_id(&self) -> String {
        let mut hasher = Sha256::new();
        for (index, part) in [&self.project, &self.file, &self.suite, &self.name]
            .into_iter()
            .enumerate()
        {
            if index > 0 {
                hasher.update([0x1f]);
            }
            hasher.update(part.as_bytes());
        }
        crate::encode_hex(&hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_identity_same_id() {
        let a = TestIdentity::new("//p", "test/a_test.exs", "A", "test one");
        let b = TestIdentity::new("//p", "test/a_test.exs", "A", "test one");
        assert_eq!(a.test_id(), b.test_id());
        assert_eq!(a.test_id().len(), 64);
    }

    #[test]
    fn parts_do_not_bleed_into_each_other() {
        let a = TestIdentity::new("//p", "a", "bc", "d");
        let b = TestIdentity::new("//p", "ab", "c", "d");
        assert_ne!(a.test_id(), b.test_id());
    }

    #[test]
    fn windows_separators_are_normalized() {
        let a = TestIdentity::new("//p", "tests\\unit\\a.rs", "", "t");
        let b = TestIdentity::new("//p", "tests/unit/a.rs", "", "t");
        assert_eq!(a.test_id(), b.test_id());
    }
}
