//! Who may read and write the cache.
//!
//! Reads: any authenticated CI token for the repository, and an IAP viewer
//! whose email domain is allowed. Writes: a static development token, or a CI
//! token whose workflow ref matches `ASTER_SRV_CACHE_WRITERS`. A pull-request
//! ref does not match the default, so a branch cannot poison the cache.

use crate::auth::Identity;
use crate::config::Config;
use crate::iap::{IapVerifier, Viewer};

/// A caller allowed to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub repository: String,
    pub may_write: bool,
}

/// Why a write was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct WriteDenied;

impl std::fmt::Display for WriteDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this caller may not write the cache")
    }
}

/// A CI caller. Static tokens may write; OIDC tokens write only when their
/// ref matches the writer patterns.
pub fn ci_reader(config: &Config, identity: &Identity) -> Reader {
    Reader {
        repository: identity.repository.clone(),
        may_write: identity.static_token
            || config
                .cache_writers
                .allows(&identity.repository, identity.workflow_ref.as_deref()),
    }
}

/// An IAP viewer may look up `repo`. Viewers never write. The verifier has
/// already applied `ASTER_SRV_UI_EMAIL_DOMAINS`.
pub fn iap_reader(
    iap: &IapVerifier,
    viewer: &Viewer,
    repo: &str,
    allowlist: &crate::config::RepositoryAllowlist,
) -> Result<Reader, &'static str> {
    if !iap.allows_email(&viewer.email) {
        return Err("this viewer may not read the cache");
    }
    if !allowlist.allows(repo) {
        return Err("this repository may not use the cache");
    }
    Ok(Reader {
        repository: repo.to_string(),
        may_write: false,
    })
}

/// Whether this reader may write.
pub fn require_writer(reader: &Reader) -> Result<(), WriteDenied> {
    if reader.may_write {
        Ok(())
    } else {
        Err(WriteDenied)
    }
}
