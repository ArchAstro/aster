//! Who may read and write the cache.
//!
//! Reads: any authenticated CI token for the repository, and a signed-in
//! person (`aster cache login`) whose email domain is allowed, for any
//! repository the server serves. Writes: a static development token, or a CI
//! token whose `ref` claim matches `ASTER_SRV_CACHE_WRITERS` and, when
//! `ASTER_SRV_CACHE_WRITER_WORKFLOWS` is set, whose `job_workflow_ref`
//! matches one of its prefixes. A pull-request ref does not match the
//! default, so a branch cannot poison the cache. People never write.

use crate::auth::Identity;
use crate::config::Config;
use crate::iap::IapVerifier;
use crate::person::Person;

/// A caller allowed to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub repository: String,
    pub may_write: bool,
    /// Recorded on a commit. Empty for a person, who cannot write.
    pub writer: Writer,
    pub caller: Caller,
}

/// Which kind of token the reader holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A CI token: confined to its own repository.
    Ci,
    /// A signed-in person: names the repository in the request.
    Person,
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

/// Whether the job's workflow may write: always when no workflow prefix is
/// configured (and for static tokens), otherwise only on a prefix match.
pub fn workflow_allowed(config: &Config, identity: &Identity) -> bool {
    identity.static_token
        || config.writer_workflows.as_ref().is_none_or(|workflows| {
            identity
                .job_workflow
                .as_deref()
                .is_some_and(|workflow| workflows.allows(workflow))
        })
}

/// Whether the run was triggered by a push. Pull requests (also from forks
/// and `pull_request_target`), merge queues and manual dispatches are not
/// trusted to write the cache, whatever ref they carry.
fn pushed(identity: &Identity) -> bool {
    identity.static_token || identity.event_name.as_deref() == Some("push")
}

/// Whether a CI token may upload test results. Results come from pull
/// requests too (a retry that passes there is the flake signal), so unlike
/// cache writes this does not require a push; it refuses only
/// `pull_request_target`, which runs with the base repository's secrets
/// against the pull request's code, and a workflow outside the configured
/// writer workflows.
pub fn results_writer_allowed(config: &Config, identity: &Identity) -> bool {
    workflow_allowed(config, identity)
        && (identity.static_token || identity.event_name.as_deref() != Some("pull_request_target"))
}

/// A CI caller. Static tokens may write; OIDC tokens write only when their
/// ref matches the writer patterns, their workflow is allowed and the run
/// was triggered by a push.
pub fn ci_reader(config: &Config, identity: &Identity) -> Reader {
    Reader {
        repository: identity.repository.clone(),
        may_write: identity.static_token
            || (config
                .cache_writers
                .allows(&identity.repository, identity.git_ref.as_deref())
                && workflow_allowed(config, identity)
                && pushed(identity)),
        writer: Writer::from_identity(identity),
        caller: Caller::Ci,
    }
}

/// A signed-in person may look up `repo`. People never write. The domain
/// rule is applied again here, so narrowing `ASTER_SRV_UI_EMAIL_DOMAINS`
/// takes effect on tokens already issued.
pub fn person_reader(
    iap: &IapVerifier,
    person: &Person,
    repo: &str,
    allowlist: &crate::config::RepositoryAllowlist,
) -> Result<Reader, &'static str> {
    if !iap.allows_email(&person.email) {
        return Err("this person may not read the cache");
    }
    if !allowlist.allows(repo) {
        return Err("this repository may not use the cache");
    }
    Ok(Reader {
        repository: repo.to_string(),
        may_write: false,
        writer: Writer::default(),
        caller: Caller::Person,
    })
}

/// Whether a signed-in person may browse the cache: the domain rule and
/// a repository the server serves.
pub fn person_browser(
    iap: &IapVerifier,
    person: &Person,
    repo: &str,
    allowlist: &crate::config::RepositoryAllowlist,
) -> Result<(), &'static str> {
    if !iap.allows_email(&person.email) {
        return Err("this person may not browse the cache");
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
            "ASTER_SRV_ALLOW_ANY_WRITER_WORKFLOW" => Some("1".into()),
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
            run_attempt: Some("1".into()),
            sha: Some("abc".into()),
            workflow: Some("wf".into()),
            job_workflow: Some(
                "ArchAstro/firstlanding/.github/workflows/ci.yml@refs/heads/main".into(),
            ),
            event_name: Some("push".into()),
            static_token: false,
        }
    }

    #[test]
    fn writer_workflows_limit_who_may_write_and_unset_limits_nobody() {
        let main = identity(Some("refs/heads/main"));
        // Unset: today's behavior, any workflow on the right ref may write.
        assert!(ci_reader(&config(&[]), &main).may_write);
        let limited = config(&[(
            "ASTER_SRV_CACHE_WRITER_WORKFLOWS",
            "ArchAstro/firstlanding/.github/workflows/ci.yml@",
        )]);
        assert!(ci_reader(&limited, &main).may_write);
        let mut other = identity(Some("refs/heads/main"));
        other.job_workflow =
            Some("ArchAstro/firstlanding/.github/workflows/evil.yml@refs/heads/main".into());
        assert!(!ci_reader(&limited, &other).may_write);
        // A token without the claim cannot satisfy a configured restriction.
        other.job_workflow = None;
        assert!(!ci_reader(&limited, &other).may_write);
        // The right workflow on a pull-request ref still cannot write.
        assert!(!ci_reader(&limited, &identity(Some("refs/pull/7/merge"))).may_write);
        // A static development token is not an Actions job and is exempt.
        let mut dev = identity(Some("refs/heads/main"));
        dev.static_token = true;
        dev.job_workflow = None;
        assert!(ci_reader(&limited, &dev).may_write);
        assert!(workflow_allowed(&limited, &dev));
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
    fn people_read_and_browse_served_repos_in_allowed_domains_and_never_write() {
        let config = config(&[]);
        let iap = IapVerifier::new(&config);
        let person = |email: &str| Person {
            subject: "accounts.google.com:1".into(),
            email: email.into(),
            issued_at: 0,
        };
        let repo = "ArchAstro/firstlanding";
        let allow = &config.repositories;
        assert!(person_browser(&iap, &person("ada@example.com"), repo, allow).is_ok());
        assert!(person_browser(&iap, &person("eve@other.test"), repo, allow).is_err());
        assert!(person_browser(&iap, &person("ada@example.com"), "evil/x", allow).is_err());
        let reader = person_reader(&iap, &person("ada@example.com"), repo, allow).unwrap();
        assert!(!reader.may_write);
        assert_eq!(reader.caller, Caller::Person);
        assert!(person_reader(&iap, &person("eve@other.test"), repo, allow).is_err());
        assert!(person_reader(&iap, &person("ada@example.com"), "evil/x", allow).is_err());
    }

    #[test]
    fn a_ci_token_browses_only_its_own_repository() {
        let id = identity(Some("refs/pull/1/merge"));
        assert!(ci_browser(&id, "ArchAstro/firstlanding").is_ok());
        assert!(ci_browser(&id, "ArchAstro/other").is_err());
    }
}
