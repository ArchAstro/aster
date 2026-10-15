//! Configuration from the environment.

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};

/// GitHub Actions' OIDC token issuer.
pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

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
        if iap_audience.as_deref().is_some_and(|a| !a.starts_with('/')) {
            bail!(
                "ASTER_SRV_IAP_AUDIENCE must look like /projects/NUMBER/global/backendServices/ID \
                 (or end in / to match any backend service of a project)"
            );
        }
        Ok(Self {
            listen,
            database_url,
            audience: var("ASTER_SRV_AUDIENCE").unwrap_or_else(|| "aster-srv".to_string()),
            issuer: var("ASTER_SRV_ISSUER").unwrap_or_else(|| GITHUB_ISSUER.to_string()),
            repositories,
            static_tokens,
            raw_bucket: var("ASTER_SRV_RAW_BUCKET"),
            retention_days,
            default_branch: var("ASTER_SRV_DEFAULT_BRANCH").unwrap_or_else(|| "main".to_string()),
            broken_runs,
            max_body_bytes,
            iap_audience,
            ui_email_domains,
            ui_dev_email: var("ASTER_SRV_UI_DEV_EMAIL").map(|email| email.trim().to_string()),
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
