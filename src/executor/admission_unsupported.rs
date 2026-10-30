//! Compatibility boundary for platforms without host execution admission.
//! Their existing executor and process supervision remain in use.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Host admission is currently available on Unix only.
pub fn enable(_executable: PathBuf) {}

/// No private execution supervisor is installed on this platform.
pub fn internal_invocation() -> Option<Result<i32>> {
    None
}

pub(super) struct Host;
pub(super) struct Permit;

impl Host {
    pub(super) fn configured() -> Result<Option<Arc<Self>>> {
        Ok(None)
    }

    pub(super) fn acquire(
        &self,
        _resources: &[String],
        _workspace: &Path,
        _address: &str,
    ) -> Result<Permit> {
        unreachable!("host admission is not enabled on this platform")
    }
}
