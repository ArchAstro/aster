//! Configuration from the environment.

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};

/// GitHub Actions' OIDC token issuer.
pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// A Google IAP audience, `/projects/NUMBER/global/backendServices/ID`, or
/// with the ID left off (a trailing `/`) to stand for any backend service of
/// the project. Both NUMBER and ID are nonempty runs of digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IapAudience<'a> {
    pub project: &'a str,
    /// `None` for the project-wide prefix form.
    pub backend_service: Option<&'a str>,
}

impl<'a> IapAudience<'a> {
    pub fn parse(audience: &'a str) -> Option<Self> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let rest = audience.strip_prefix("/projects/")?;
        let (project, rest) = rest.split_once('/')?;
        let service = rest.strip_prefix("global/backendServices/")?;
        if !digits(project) {
            return None;
        }
        let backend_service = match service {
            "" => None,
            id if digits(id) => Some(id),
            _ => return None,
        };
        Some(Self {
            project,
            backend_service,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// `ASTER_SRV_LISTEN`, default `0.0.0.0:8080`.
    pub listen: SocketAddr,
    /// `ASTER_SRV_DATABASE_URL`, a Postgres URL. Required.
    pub database_url: String,
    /// `ASTER_SRV_AUDIENCE`: the OIDC audience CI requests tokens for.
    /// Default `aster-srv`.
    pub audience: String,
    /// `ASTER_SRV_ISSUER`, default GitHub Actions.
    pub issuer: String,
    /// `ASTER_SRV_REPOSITORIES`: comma-separated `owner/name` or `owner/*`.
    /// Required; a repository not listed is refused.
    pub repositories: RepositoryAllowlist,
    /// `ASTER_SRV_STATIC_TOKENS`: comma-separated `owner/name=token` pairs
    /// accepted in place of an OIDC token. For local development and tests
    /// only.
    pub static_tokens: Vec<StaticToken>,
    /// `ASTER_SRV_RAW_BUCKET`: a GCS bucket that keeps each upload as sent.
    /// Unset keeps nothing.
    pub raw_bucket: Option<String>,
    /// `ASTER_SRV_RETENTION_DAYS`, default 30.
    pub retention_days: u32,
    /// `ASTER_SRV_DEFAULT_BRANCH`: the branch whose runs decide "broken" and
    /// "likely flaky". Default `main`.
    pub default_branch: String,
    /// `ASTER_SRV_BROKEN_RUNS`: how many consecutive default-branch runs must
    /// fail with the same failure hash before a test is broken. At least 3;
    /// default 3.
    pub broken_runs: u32,
    /// `ASTER_SRV_MAX_BODY_BYTES`: the largest upload accepted, compressed
    /// or not. Default 64 MiB.
    pub max_body_bytes: usize,
    /// `ASTER_SRV_IAP_AUDIENCE`: the Google IAP audience the dashboard (`/ui`)
    /// accepts, `/projects/NUMBER/global/backendServices/ID` (or, for GKE
    /// ingress, which assigns the ID, a prefix ending in `/` such as
    /// `/projects/NUMBER/global/backendServices/`). Unset disables the
    /// dashboard unless `ASTER_SRV_UI_DEV_EMAIL` is set.
    pub iap_audience: Option<String>,
    /// `ASTER_SRV_UI_EMAIL_DOMAINS`: comma-separated domains (`example.com`,
    /// or `*.example.com` for subdomains) whose emails may view the
    /// dashboard. Unset lets every identity that passes IAP in.
    pub ui_email_domains: Option<EmailDomains>,
    /// `ASTER_SRV_UI_DEV_EMAIL`: the viewer the dashboard assumes when no IAP
    /// audience is configured. For local development only.
    pub ui_dev_email: Option<String>,
    /// When tests enter and leave quarantine.
    pub quarantine: QuarantineRules,
    /// `ASTER_SRV_WEBHOOKS`: where state changes are sent. Empty sends
    /// nothing.
    pub webhooks: Vec<crate::webhooks::Endpoint>,
    /// `ASTER_SRV_CACHE_BUCKET`: the GCS bucket that holds `cas/<sha256>`
    /// blobs. Unset disables the cache.
    pub cache_bucket: Option<String>,
    /// `ASTER_SRV_CACHE_DAYS`: drop manifests unused this long, and blobs
    /// no remaining manifest names. Default 14.
    pub cache_days: u32,
    /// `ASTER_SRV_CACHE_WRITERS`: `repo=ref` patterns that may upload and
    /// commit. Default `*=refs/heads/<default branch>`.
    pub cache_writers: CacheWriters,
}

/// When tests enter and leave quarantine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuarantineRules {
    /// `ASTER_SRV_QUARANTINE_RUNS`: a test enters quarantine once this many
    /// different runs in the last 7 days hold a definite flake of it. 0
    /// turns automatic quarantine off. Default 2.
    pub runs: u32,
    /// `ASTER_SRV_QUARANTINE_RELEASE_PASSES`: consecutive passes, while
    /// quarantined, that release a test. Default 20.
    pub release_passes: u32,
    /// `ASTER_SRV_QUARANTINE_DAYS`: the longest a test stays quarantined,
    /// at most the retention period. Default 30.
    pub days: u32,
}

/// A fixed bearer token standing in for one repository.
#[derive(Clone)]
pub struct StaticToken {
    pub repository: String,
    pub token: String,
}

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticToken")
            .field("repository", &self.repository)
            .field("token", &"[redacted]")
            .finish()
    }
}

impl Config {
    /// Read the configuration through `env`, which looks up one variable.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let var = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        let listen = var("ASTER_SRV_LISTEN")
            .unwrap_or_else(|| "0.0.0.0:8080".to_string())
            .parse()
            .context("ASTER_SRV_LISTEN must be host:port")?;
        let database_url =
            var("ASTER_SRV_DATABASE_URL").context("ASTER_SRV_DATABASE_URL is required")?;
        let repositories = RepositoryAllowlist::parse(&var("ASTER_SRV_REPOSITORIES").context(
            "ASTER_SRV_REPOSITORIES is required: the repositories allowed to upload, \
                 such as ArchAstro/firstlanding or ArchAstro/*",
        )?)?;
        let static_tokens = match var("ASTER_SRV_STATIC_TOKENS") {
            None => Vec::new(),
            Some(value) => value
                .split(',')
                .map(str::trim)
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (repository, token) = pair
                        .split_once('=')
                        .context("ASTER_SRV_STATIC_TOKENS entries are owner/name=token")?;
                    if token.len() < 16 {
                        bail!("static tokens must be at least 16 characters");
                    }
                    Ok(StaticToken {
                        repository: repository.trim().to_string(),
                        token: token.trim().to_string(),
                    })
                })
                .collect::<Result<_>>()?,
        };
        let retention_days = match var("ASTER_SRV_RETENTION_DAYS") {
            Some(days) => days
                .parse()
                .ok()
                .filter(|days| *days > 0)
                .context("ASTER_SRV_RETENTION_DAYS must be a positive number of days")?,
            None => 30,
        };
        let broken_runs = match var("ASTER_SRV_BROKEN_RUNS") {
            Some(runs) => runs
                .parse()
                .ok()
                .filter(|runs| *runs >= 3)
                .context("ASTER_SRV_BROKEN_RUNS must be a number of runs, at least 3")?,
            None => 3,
        };
        let max_body_bytes = match var("ASTER_SRV_MAX_BODY_BYTES") {
            Some(bytes) => bytes
                .parse()
                .context("ASTER_SRV_MAX_BODY_BYTES must be a number of bytes")?,
            None => 64 * 1024 * 1024,
        };
        let ui_email_domains = match var("ASTER_SRV_UI_EMAIL_DOMAINS") {
            Some(domains) => Some(EmailDomains::parse(&domains)?),
            None => None,
        };
        let iap_audience =
            var("ASTER_SRV_IAP_AUDIENCE").map(|audience| audience.trim().to_string());
        if iap_audience
            .as_deref()
            .is_some_and(|a| IapAudience::parse(a).is_none())
        {
            bail!(
                "ASTER_SRV_IAP_AUDIENCE must look like /projects/NUMBER/global/backendServices/ID \
                 (or end in / to match any backend service of a project)"
            );
        }
        let number = |name: &str, default: u32, min: u32, max: u32| -> Result<u32> {
            match var(name) {
                None => Ok(default),
                Some(value) => value
                    .trim()
                    .parse()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .with_context(|| format!("{name} must be a number from {min} to {max}")),
            }
        };
        let quarantine = QuarantineRules {
            runs: number("ASTER_SRV_QUARANTINE_RUNS", 2, 0, 1000)?,
            release_passes: number("ASTER_SRV_QUARANTINE_RELEASE_PASSES", 20, 1, 10_000)?,
            days: number(
                "ASTER_SRV_QUARANTINE_DAYS",
                30.min(retention_days),
                1,
                retention_days,
            )?,
        };
        let webhooks = match var("ASTER_SRV_WEBHOOKS") {
            Some(value) => crate::webhooks::Endpoint::parse_all(&value)?,
            None => Vec::new(),
        };
        let default_branch = var("ASTER_SRV_DEFAULT_BRANCH").unwrap_or_else(|| "main".to_string());
        let cache_days = match var("ASTER_SRV_CACHE_DAYS") {
            Some(days) => days
                .parse()
                .ok()
                .filter(|days| *days > 0)
                .context("ASTER_SRV_CACHE_DAYS must be a positive number of days")?,
            None => 14,
        };
        let cache_writers =
            CacheWriters::parse(var("ASTER_SRV_CACHE_WRITERS").as_deref(), &default_branch)?;
        Ok(Self {
            listen,
            database_url,
            audience: var("ASTER_SRV_AUDIENCE").unwrap_or_else(|| "aster-srv".to_string()),
            issuer: var("ASTER_SRV_ISSUER").unwrap_or_else(|| GITHUB_ISSUER.to_string()),
            repositories,
            static_tokens,
            raw_bucket: var("ASTER_SRV_RAW_BUCKET"),
            retention_days,
            default_branch,
            broken_runs,
            max_body_bytes,
            iap_audience,
            ui_email_domains,
            ui_dev_email: var("ASTER_SRV_UI_DEV_EMAIL").map(|email| email.trim().to_string()),
            quarantine,
            webhooks,
            cache_bucket: var("ASTER_SRV_CACHE_BUCKET"),
            cache_days,
            cache_writers,
        })
    }
}

/// Repositories allowed to use the server.
#[derive(Debug, Clone)]
pub struct RepositoryAllowlist(Vec<String>);

impl RepositoryAllowlist {
    pub fn parse(value: &str) -> Result<Self> {
        let entries: Vec<String> = value
            .split(',')
            .map(|entry| entry.trim().to_ascii_lowercase())
            .filter(|entry| !entry.is_empty())
            .collect();
        for entry in &entries {
            let Some((owner, name)) = entry.split_once('/') else {
                bail!("repository '{entry}' must be owner/name or owner/*");
            };
            if owner.is_empty() || name.is_empty() || owner == "*" || name.contains('/') {
                bail!("repository '{entry}' must be owner/name or owner/*");
            }
        }
        if entries.is_empty() {
            bail!("ASTER_SRV_REPOSITORIES lists no repository");
        }
        Ok(Self(entries))
    }

    /// Whether `repository` (`owner/name`, any case) is allowed.
    pub fn allows(&self, repository: &str) -> bool {
        let repository = repository.to_ascii_lowercase();
        let Some((owner, _)) = repository.split_once('/') else {
            return false;
        };
        self.0.iter().any(|entry| {
            *entry == repository
                || entry
                    .strip_suffix("/*")
                    .is_some_and(|allowed_owner| allowed_owner == owner)
        })
    }
}

/// `repo=ref` patterns that may write the cache. `*` matches any repository.
/// A ref pattern matches exactly, or as a prefix when it ends in `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheWriters {
    patterns: Vec<(String, String)>,
}

impl CacheWriters {
    /// `None` is the default: any allowed repository, on `refs/heads/<branch>`.
    pub fn parse(value: Option<&str>, default_branch: &str) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self {
                patterns: vec![("*".into(), format!("refs/heads/{default_branch}"))],
            });
        };
        let mut patterns = Vec::new();
        for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (repo, pattern) = entry.split_once('=').with_context(|| {
                format!("ASTER_SRV_CACHE_WRITERS entry '{entry}' must be repo=ref")
            })?;
            let repo = repo.trim();
            let pattern = pattern.trim();
            if repo.is_empty() || pattern.is_empty() || repo.contains('*') && repo != "*" {
                bail!("ASTER_SRV_CACHE_WRITERS entry '{entry}' must be owner/name=ref or *=ref");
            }
            patterns.push((repo.to_string(), pattern.to_string()));
        }
        if patterns.is_empty() {
            bail!("ASTER_SRV_CACHE_WRITERS lists no writer");
        }
        Ok(Self { patterns })
    }

    /// Whether `workflow_ref` may write for `repository`.
    pub fn allows(&self, repository: &str, workflow_ref: Option<&str>) -> bool {
        let Some(workflow_ref) = workflow_ref else {
            return false;
        };
        self.patterns.iter().any(|(repo, pattern)| {
            (*repo == "*" || repo.eq_ignore_ascii_case(repository))
                && ref_matches(pattern, workflow_ref)
        })
    }
}

fn ref_matches(pattern: &str, workflow_ref: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => !prefix.is_empty() && workflow_ref.starts_with(prefix),
        None => pattern == workflow_ref,
    }
}

/// Email domains allowed to view the dashboard.
#[derive(Debug, Clone)]
pub struct EmailDomains(Vec<String>);

impl EmailDomains {
    /// `example.com` matches that domain; `*.example.com` matches its
    /// subdomains (not `example.com` itself).
    pub fn parse(value: &str) -> Result<Self> {
        let entries: Vec<String> = value
            .split(',')
            .map(|entry| entry.trim().to_ascii_lowercase())
            .filter(|entry| !entry.is_empty())
            .collect();
        for entry in &entries {
            let domain = entry.strip_prefix("*.").unwrap_or(entry);
            if domain.is_empty() || domain.contains(['@', '*', '/']) {
                bail!("email domain '{entry}' must be example.com or *.example.com");
            }
        }
        if entries.is_empty() {
            bail!("ASTER_SRV_UI_EMAIL_DOMAINS lists no domain");
        }
        Ok(Self(entries))
    }

    /// Whether `email` (any case) is at an allowed domain.
    pub fn allows(&self, email: &str) -> bool {
        let email = email.to_ascii_lowercase();
        let Some((local, domain)) = email.rsplit_once('@') else {
            return false;
        };
        !local.is_empty()
            && self.0.iter().any(|entry| match entry.strip_prefix("*.") {
                Some(suffix) => domain
                    .strip_suffix(suffix)
                    .is_some_and(|rest| rest.ends_with('.') && rest.len() > 1),
                None => *entry == domain,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    const REQUIRED: [(&str, &str); 2] = [
        ("ASTER_SRV_DATABASE_URL", "postgres://localhost/aster"),
        ("ASTER_SRV_REPOSITORIES", "ArchAstro/*"),
    ];

    #[test]
    fn defaults() {
        let config = Config::from_env(env_of(&REQUIRED)).unwrap();
        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.audience, "aster-srv");
        assert_eq!(config.issuer, GITHUB_ISSUER);
        assert_eq!(config.retention_days, 30);
        assert_eq!(config.default_branch, "main");
        assert_eq!(config.broken_runs, 3);
        assert_eq!(config.max_body_bytes, 64 * 1024 * 1024);
        assert!(config.static_tokens.is_empty());
        assert!(config.raw_bucket.is_none());
        assert!(config.iap_audience.is_none());
        assert!(config.ui_email_domains.is_none());
        assert!(config.ui_dev_email.is_none());
        assert_eq!(
            config.quarantine,
            QuarantineRules {
                runs: 2,
                release_passes: 20,
                days: 30
            }
        );
        assert!(config.webhooks.is_empty());
        assert!(config.cache_bucket.is_none());
        assert_eq!(config.cache_days, 14);
        assert!(config
            .cache_writers
            .allows("ArchAstro/aster", Some("refs/heads/main")));
        assert!(!config
            .cache_writers
            .allows("ArchAstro/aster", Some("refs/pull/7/merge")));
        assert!(!config.cache_writers.allows("ArchAstro/aster", None));
    }

    #[test]
    fn reads_the_quarantine_rules() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_QUARANTINE_RUNS", "0"));
        pairs.push(("ASTER_SRV_QUARANTINE_RELEASE_PASSES", "5"));
        pairs.push(("ASTER_SRV_QUARANTINE_DAYS", "14"));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(
            config.quarantine,
            QuarantineRules {
                runs: 0,
                release_passes: 5,
                days: 14
            }
        );
        // Quarantine never outlives the results that justify it.
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_RETENTION_DAYS", "10"));
        assert_eq!(
            Config::from_env(env_of(&pairs)).unwrap().quarantine.days,
            10
        );
        pairs.push(("ASTER_SRV_QUARANTINE_DAYS", "11"));
        assert!(Config::from_env(env_of(&pairs)).is_err());
        for (name, bad) in [
            ("ASTER_SRV_QUARANTINE_RUNS", "-1"),
            ("ASTER_SRV_QUARANTINE_RELEASE_PASSES", "0"),
            ("ASTER_SRV_QUARANTINE_DAYS", "0"),
        ] {
            let mut pairs = REQUIRED.to_vec();
            pairs.push((name, bad));
            assert!(Config::from_env(env_of(&pairs)).is_err(), "{name}={bad}");
        }
    }

    #[test]
    fn reads_the_dashboard_settings() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push((
            "ASTER_SRV_IAP_AUDIENCE",
            "/projects/123456/global/backendServices/",
        ));
        pairs.push(("ASTER_SRV_UI_EMAIL_DOMAINS", "Example.com, *.corp.example"));
        pairs.push(("ASTER_SRV_UI_DEV_EMAIL", "me@example.com"));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(
            config.iap_audience.as_deref(),
            Some("/projects/123456/global/backendServices/")
        );
        let domains = config.ui_email_domains.unwrap();
        assert!(domains.allows("a@example.com"));
        assert_eq!(config.ui_dev_email.as_deref(), Some("me@example.com"));

        for (name, bad) in [
            ("ASTER_SRV_IAP_AUDIENCE", "no-slash"),
            ("ASTER_SRV_IAP_AUDIENCE", "/"),
            ("ASTER_SRV_IAP_AUDIENCE", "/projects/"),
            ("ASTER_SRV_IAP_AUDIENCE", "/projects/123456/"),
            (
                "ASTER_SRV_IAP_AUDIENCE",
                "/projects/123456/global/backendServices",
            ),
            (
                "ASTER_SRV_IAP_AUDIENCE",
                "/projects/abc/global/backendServices/",
            ),
            (
                "ASTER_SRV_IAP_AUDIENCE",
                "/projects/123456/global/backendServices/9/x",
            ),
            ("ASTER_SRV_UI_EMAIL_DOMAINS", "a@b.com"),
            ("ASTER_SRV_UI_EMAIL_DOMAINS", "*."),
        ] {
            let mut pairs = REQUIRED.to_vec();
            pairs.push((name, bad));
            assert!(Config::from_env(env_of(&pairs)).is_err(), "{bad}");
        }
    }

    #[test]
    fn email_domains_match_exactly_or_by_wildcard() {
        let domains = EmailDomains::parse("example.com, *.gserviceaccount.com").unwrap();
        assert!(domains.allows("Ann@Example.com"));
        assert!(domains.allows("ci@proj.iam.gserviceaccount.com"));
        assert!(!domains.allows("ci@gserviceaccount.com"));
        assert!(!domains.allows("ci@evilgserviceaccount.com"));
        assert!(!domains.allows("a@sub.example.com"));
        assert!(!domains.allows("a@example.com.evil.io"));
        assert!(!domains.allows("example.com"));
        assert!(!domains.allows("@example.com"));
        assert!(EmailDomains::parse(" , ").is_err());
    }

    #[test]
    fn reads_the_classifier_settings() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_DEFAULT_BRANCH", "trunk"));
        pairs.push(("ASTER_SRV_BROKEN_RUNS", "5"));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(config.default_branch, "trunk");
        assert_eq!(config.broken_runs, 5);
        for bad in ["2", "0", "many"] {
            let mut pairs = REQUIRED.to_vec();
            pairs.push(("ASTER_SRV_BROKEN_RUNS", bad));
            assert!(Config::from_env(env_of(&pairs)).is_err(), "{bad}");
        }
    }

    #[test]
    fn requires_a_database_and_an_allowlist() {
        let error = Config::from_env(env_of(&REQUIRED[1..])).unwrap_err();
        assert!(error.to_string().contains("ASTER_SRV_DATABASE_URL"));
        let error = Config::from_env(env_of(&REQUIRED[..1])).unwrap_err();
        assert!(error.to_string().contains("ASTER_SRV_REPOSITORIES"));
    }

    #[test]
    fn reads_static_tokens_without_printing_them() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push((
            "ASTER_SRV_STATIC_TOKENS",
            "ArchAstro/aster=0123456789abcdef0123",
        ));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(config.static_tokens[0].repository, "ArchAstro/aster");
        assert!(!format!("{config:?}").contains("0123456789abcdef"));

        pairs.pop();
        pairs.push(("ASTER_SRV_STATIC_TOKENS", "ArchAstro/aster=short"));
        assert!(Config::from_env(env_of(&pairs)).is_err());
    }

    #[test]
    fn allowlist_matches_names_and_owners_case_insensitively() {
        let allow = RepositoryAllowlist::parse("ArchAstro/firstlanding, other/*").unwrap();
        assert!(allow.allows("archastro/FirstLanding"));
        assert!(allow.allows("Other/anything"));
        assert!(!allow.allows("ArchAstro/aster"));
        assert!(!allow.allows("evil/firstlanding"));
        assert!(!allow.allows("nonsense"));
        for bad in ["", "*/*", "owner", "a/b/c"] {
            assert!(RepositoryAllowlist::parse(bad).is_err(), "{bad}");
        }
    }
}
