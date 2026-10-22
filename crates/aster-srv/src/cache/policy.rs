//! Who may read and write the cache.
//!
//! Reads: any authenticated CI token for the repository, and an IAP viewer
//! whose email domain is allowed. Writes: a static development token, or a CI
//! token whose `ref` claim matches `ASTER_SRV_CACHE_WRITERS`. A pull-request
//! ref does not match the default, so a branch cannot poison the cache.

use crate::auth::Identity;
use crate::config::Config;
use crate::iap::{IapVerifier, Viewer};

/// A caller allowed to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub repository: String,
    pub may_write: bool,
    /// Recorded on a commit. Empty for an IAP viewer, who cannot write.
    pub writer: Writer,
}

/// Who wrote a manifest, from the verified OIDC claims.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Writer {
    /// The git `ref` claim.
    pub git_ref: Option<String>,
    pub run_id: Option<String>,
    pub sha: Option<String>,
    /// The `workflow_ref` claim.
    pub workflow: Option<String>,
}

impl Writer {
    pub fn from_identity(identity: &Identity) -> Self {
        Self {
            git_ref: identity.git_ref.clone(),
            run_id: identity.run_id.clone(),
            sha: identity.sha.clone(),
            workflow: identity.workflow.clone(),
        }
    }
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
                .allows(&identity.repository, identity.git_ref.as_deref()),
        writer: Writer::from_identity(identity),
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
        writer: Writer::default(),
    })
}

/// Whether an IAP viewer may browse the cache: the viewer domain rule and
/// a repository the server serves.
pub fn iap_browser(
    iap: &IapVerifier,
    viewer: &Viewer,
    repo: &str,
    allowlist: &crate::config::RepositoryAllowlist,
) -> Result<(), &'static str> {
    if !iap.allows_email(&viewer.email) {
        return Err("this viewer may not browse the cache");
    }
    if !allowlist.allows(repo) {
        return Err("this repository may not use the cache");
    }
    Ok(())
}

/// A CI token may browse its own repository's cache.
pub fn ci_browser(identity: &Identity, repo: &str) -> Result<(), &'static str> {
    if identity.repository == repo {
        Ok(())
    } else {
        Err("the token is for a different repository")
    }
}

/// Whether this reader may write.
pub fn require_writer(reader: &Reader) -> Result<(), WriteDenied> {
    if reader.may_write {
        Ok(())
    } else {
        Err(WriteDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &[(&str, &str)]) -> Config {
        Config::from_env(|name| match name {
            "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
            "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
            "ASTER_SRV_UI_EMAIL_DOMAINS" => Some("example.com".into()),
            other => extra
                .iter()
                .find(|(key, _)| *key == other)
                .map(|(_, value)| value.to_string()),
        })
        .unwrap()
    }

    fn identity(git_ref: Option<&str>) -> Identity {
        Identity {
            repository: "ArchAstro/firstlanding".into(),
            git_ref: git_ref.map(str::to_string),
            run_id: Some("9".into()),
            sha: Some("abc".into()),
            workflow: Some("wf".into()),
            static_token: false,
        }
    }

    #[test]
    fn writer_patterns_match_the_ref_claim_not_the_workflow() {
        let config = config(&[(
            "ASTER_SRV_CACHE_WRITERS",
            "ArchAstro/firstlanding=refs/heads/main",
        )]);
        assert!(ci_reader(&config, &identity(Some("refs/heads/main"))).may_write);
        assert!(!ci_reader(&config, &identity(Some("refs/pull/7/merge"))).may_write);
        assert!(!ci_reader(&config, &identity(None)).may_write);
        // The workflow claim never stands in for the ref.
        let mut by_workflow = identity(None);
        by_workflow.workflow = Some("refs/heads/main".into());
        assert!(!ci_reader(&config, &by_workflow).may_write);
    }

    #[test]
    fn a_reader_carries_the_writer_claims_for_commit() {
        let reader = ci_reader(&config(&[]), &identity(Some("refs/heads/main")));
        assert_eq!(
            reader.writer,
            Writer {
                git_ref: Some("refs/heads/main".into()),
                run_id: Some("9".into()),
                sha: Some("abc".into()),
                workflow: Some("wf".into()),
            }
        );
    }

    #[test]
    fn browsing_allows_domain_viewers_on_served_repos() {
        let config = config(&[]);
        let iap = IapVerifier::new(&config);
        let viewer = |email: &str| Viewer {
            email: email.into(),
        };
        let repo = "ArchAstro/firstlanding";
        let allow = &config.repositories;
        assert!(iap_browser(&iap, &viewer("ada@example.com"), repo, allow).is_ok());
        assert!(iap_browser(&iap, &viewer("eve@other.test"), repo, allow).is_err());
        assert!(iap_browser(&iap, &viewer("ada@example.com"), "evil/x", allow).is_err());
    }

    #[test]
    fn a_ci_token_browses_only_its_own_repository() {
        let id = identity(Some("refs/pull/1/merge"));
        assert!(ci_browser(&id, "ArchAstro/firstlanding").is_ok());
        assert!(ci_browser(&id, "ArchAstro/other").is_err());
    }
}
