//! Configuration from the environment.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// GitHub Actions' OIDC token issuer.
pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// A Google IAP audience, `/projects/NUMBER/global/backendServices/ID`, or
/// with the ID left off (a trailing `/`), which parses but is not accepted
/// as a configured audience. Both NUMBER and ID are nonempty runs of digits.
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
    /// `ASTER_SRV_MAX_LARGE_BODIES`: how many large request bodies (a cache
    /// commit's manifest, a test-results upload) one replica reads at once.
    /// Each can weigh `max_body_bytes`; past the limit a request gets `503`
    /// with `Retry-After`. Default 8.
    pub max_large_bodies: usize,
    /// `ASTER_SRV_IAP_AUDIENCE`: the Google IAP audience the dashboard (`/ui`)
    /// accepts: exactly `/projects/NUMBER/global/backendServices/ID`. A
    /// project-wide prefix is refused at startup, since it would accept an
    /// assertion for any other backend service of the project. Unset
    /// disables the dashboard unless `ASTER_SRV_UI_DEV_EMAIL` is set.
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
    /// `ASTER_SRV_CACHE_SIGNER`: the pod's Workload Identity service account
    /// email, which signs blob URLs through `signBlob`. Required with a
    /// cache bucket.
    pub cache_signer: Option<String>,
    /// `ASTER_SRV_CACHE_DAYS`: drop manifests unused this long, and blobs
    /// no remaining manifest names. Default 14.
    pub cache_days: u32,
    /// `ASTER_SRV_CACHE_WRITERS`: `repo=ref` patterns that may upload and
    /// commit. Default `*=refs/heads/<default branch>`.
    pub cache_writers: CacheWriters,
    /// `ASTER_SRV_CACHE_WRITER_WORKFLOWS`: comma-separated prefixes of the
    /// `job_workflow_ref` claim (for example
    /// `ArchAstro/firstlanding/.github/workflows/ci.yml@`). When set, a CI
    /// token may write the cache or upload test results only from a job
    /// whose workflow matches one. Unset applies no workflow restriction.
    pub writer_workflows: Option<WriterWorkflows>,
    /// `ASTER_SRV_REPOSITORY_OWNER_IDS`: comma-separated `owner/name=id` or
    /// `owner/*=id`. A token for a matching repository must carry that
    /// `repository_owner_id` claim, so a repository recreated under the same
    /// name by someone else is refused. Unset pins nothing.
    pub owner_ids: OwnerIds,
    /// `ASTER_SRV_DEV_BLOB_ROUTE=1`: answer `PUT|GET /<bucket>/cas/<sha>`
    /// from the in-memory blob store, the stand-in for a signed GCS URL.
    /// For tests and local development only; unset in every deploy, which
    /// uses GCS and has no such route.
    pub dev_blob_route: bool,
    /// `ASTER_SRV_PUBLIC_URL`: the URL people reach this server at, the
    /// `iss` of the tokens `aster cache login` hands out. Unset turns
    /// person login off.
    pub public_url: Option<String>,
    /// `ASTER_SRV_CLI_TOKEN_KEYS`: `kid:secret` pairs, comma-separated. The
    /// first signs person tokens; every one verifies them, so a key is
    /// rotated out by dropping its pair. Unset turns person login off.
    pub cli_token_keys: Vec<CliTokenKey>,
    /// `ASTER_SRV_CLI_TOKEN_TTL`: seconds a person token lives, 60 to
    /// 604800. Default 43200 (12 hours).
    pub cli_token_ttl: u64,
    /// `ASTER_SRV_API_ADDR`: a second listener that serves only `/v1/*`, for
    /// the load balancer backend that has no IAP in front of it. Unset
    /// (default) leaves it off. See [`crate::http::Surface`].
    pub api_listen: Option<SocketAddr>,
    /// `ASTER_SRV_TRUSTED_PROXY_HOPS`: how many right-most `X-Forwarded-For`
    /// entries were appended by infrastructure we trust. Default 1: the
    /// Google load balancer appends `<client ip>, <forwarding rule ip>`, so
    /// the client is the second entry from the right. 0 ignores the header.
    /// See [`crate::limits::client_ip`].
    pub trusted_proxy_hops: usize,
    /// `ASTER_SRV_RATE_TOKEN_IP_PER_MIN`: `POST /v1/auth/token` requests per
    /// minute from one client IP, per replica. Default 10.
    pub rate_token_ip_per_min: u32,
    /// `ASTER_SRV_RATE_API_IP_PER_MIN`: `/v1/*` requests per minute from one
    /// client IP, per replica. Default 1200.
    pub rate_api_ip_per_min: u32,
    /// `ASTER_SRV_RATE_API_SUBJECT_PER_MIN`: authenticated `/v1/*` requests
    /// per minute for one subject (a person, or a CI repository), per
    /// replica. Default 3000.
    pub rate_api_subject_per_min: u32,
    /// `ASTER_SRV_BODY_TIMEOUT_SECS`: how long a request body may take to
    /// arrive in full. Default 60.
    pub body_timeout: Duration,
    /// `ASTER_SRV_UNAUTH_BODY_TIMEOUT_SECS`: how long the body of an
    /// unauthenticated route (`POST /v1/auth/token`, `POST /cli-auth`, both
    /// at most 8 KiB) may take to arrive, at most `body_timeout`. Default 8.
    pub unauth_body_timeout: Duration,
    /// `ASTER_SRV_MAX_CONNECTIONS_PER_IP`: requests in flight at once from
    /// one client IP (as [`crate::limits::client_ip`] resolves it; IPv6 by
    /// /64), per replica; more get `429`. With `trusted_proxy_hops` 0, where
    /// the TCP peer is the client, it also caps open connections from one
    /// peer. Default 128.
    pub max_connections_per_ip: usize,
    /// `ASTER_SRV_HANDLER_TIMEOUT_SECS`: how long a request may run from the
    /// end of its headers to its reply, body read included. Default 120.
    pub handler_timeout: Duration,
    /// `ASTER_SRV_HEADER_TIMEOUT_SECS`: how long a client may take to send a
    /// request's headers, and how long an idle connection is kept between
    /// requests. Default 20.
    pub header_timeout: Duration,
    /// `ASTER_SRV_MAX_CONNECTIONS`: open connections per listener; further
    /// ones wait in the kernel's accept queue. Default 1024.
    pub max_connections: usize,
    /// `ASTER_SRV_DB_WAIT_SECS`: how long a request waits for a database
    /// connection before failing. Default 5.
    pub db_wait: Duration,
    /// `ASTER_SRV_PERSON_TOKENS=off`: the kill switch. Every person token is
    /// refused and `/cli-auth` answers 404, whatever else is configured.
    pub person_tokens_enabled: bool,
}

/// One HS256 key for person tokens.
#[derive(Clone)]
pub struct CliTokenKey {
    pub kid: String,
    /// The decoded key: at least 32 bytes ([`MIN_KEY_BYTES`]).
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for CliTokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliTokenKey")
            .field("kid", &self.kid)
            .field("secret", &"[redacted]")
            .finish()
    }
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
    /// `ASTER_SRV_QUARANTINE_RUN_CAP`: a run holding definite flakes of more
    /// tests than this counts toward none of them, so one bad run cannot
    /// quarantine a crowd. Default 10.
    pub run_cap: u32,
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
        // Exactly one backend service: a prefix would accept an assertion IAP
        // issued for another backend service of the project.
        if iap_audience
            .as_deref()
            .is_none_or(|a| IapAudience::parse(a).is_none_or(|a| a.backend_service.is_none()))
            && iap_audience.is_some()
        {
            bail!(
                "ASTER_SRV_IAP_AUDIENCE must be the full backend-service audience, \
                 /projects/NUMBER/global/backendServices/ID"
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
            run_cap: number("ASTER_SRV_QUARANTINE_RUN_CAP", 10, 1, 100_000)?,
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
        let cache_bucket = var("ASTER_SRV_CACHE_BUCKET");
        let cache_signer = var("ASTER_SRV_CACHE_SIGNER").map(|email| email.trim().to_string());
        if cache_bucket.is_some()
            && !cache_signer
                .as_deref()
                .is_some_and(|email| email.contains('@'))
        {
            bail!("ASTER_SRV_CACHE_BUCKET needs ASTER_SRV_CACHE_SIGNER, the pod's service account email");
        }
        let cache_writers =
            CacheWriters::parse(var("ASTER_SRV_CACHE_WRITERS").as_deref(), &default_branch)?;
        let writer_workflows = match var("ASTER_SRV_CACHE_WRITER_WORKFLOWS") {
            Some(value) => Some(WriterWorkflows::parse(&value)?),
            None => None,
        };
        let allow_http_public_url = var("ASTER_SRV_ALLOW_HTTP_PUBLIC_URL").as_deref() == Some("1");
        // Naming who may write without naming the workflow lets any workflow
        // in the repository on that ref write. Say so, or opt out loudly.
        if var("ASTER_SRV_CACHE_WRITERS").is_some()
            && writer_workflows.is_none()
            && var("ASTER_SRV_ALLOW_ANY_WRITER_WORKFLOW").as_deref() != Some("1")
        {
            bail!(
                "ASTER_SRV_CACHE_WRITERS needs ASTER_SRV_CACHE_WRITER_WORKFLOWS (the workflows \
                 that may write); set ASTER_SRV_ALLOW_ANY_WRITER_WORKFLOW=1 only in development"
            );
        }
        let owner_ids = OwnerIds::parse(var("ASTER_SRV_REPOSITORY_OWNER_IDS").as_deref())?;
        let public_url = match var("ASTER_SRV_PUBLIC_URL") {
            Some(url) => {
                let url = url.trim().trim_end_matches('/').to_string();
                if !(url.starts_with("https://") || url.starts_with("http://"))
                    || url.contains(['?', '#'])
                {
                    bail!("ASTER_SRV_PUBLIC_URL must be an https URL such as https://aster-srv.example.com");
                }
                // Person tokens name this URL as issuer and audience and
                // travel to it; over http they would cross the network in
                // clear. Only a development or test server opts out.
                if url.starts_with("http://") && !allow_http_public_url {
                    bail!(
                        "ASTER_SRV_PUBLIC_URL must be https; set ASTER_SRV_ALLOW_HTTP_PUBLIC_URL=1 \
                         only on a development or test server"
                    );
                }
                Some(url)
            }
            None => None,
        };
        let cli_token_keys = match var("ASTER_SRV_CLI_TOKEN_KEYS") {
            Some(value) => parse_cli_token_keys(&value)?,
            None => Vec::new(),
        };
        let person_tokens_enabled = match var("ASTER_SRV_PERSON_TOKENS")
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("" | "on") => true,
            Some("off") => false,
            Some(_) => bail!("ASTER_SRV_PERSON_TOKENS must be on or off"),
        };
        let cli_token_ttl = match var("ASTER_SRV_CLI_TOKEN_TTL") {
            Some(value) => value
                .trim()
                .parse()
                .ok()
                .filter(|seconds| (60..=crate::person::MAX_TTL_SECONDS).contains(seconds))
                .context("ASTER_SRV_CLI_TOKEN_TTL must be a number of seconds from 60 to 43200")?,
            None => 12 * 60 * 60,
        };
        let api_listen: Option<SocketAddr> = match var("ASTER_SRV_API_ADDR") {
            Some(addr) => Some(
                addr.trim()
                    .parse()
                    .context("ASTER_SRV_API_ADDR must be host:port")?,
            ),
            None => None,
        };
        if api_listen.is_some_and(|api| api.port() != 0 && api == listen) {
            bail!("ASTER_SRV_API_ADDR must differ from ASTER_SRV_LISTEN");
        }
        let number =
            |name: &str, default: u64, range: std::ops::RangeInclusive<u64>| match var(name) {
                Some(value) => value
                    .trim()
                    .parse()
                    .ok()
                    .filter(|number| range.contains(number))
                    .with_context(|| {
                        format!(
                            "{name} must be a number from {} to {}",
                            range.start(),
                            range.end()
                        )
                    }),
                None => Ok(default),
            };
        let trusted_proxy_hops = number("ASTER_SRV_TRUSTED_PROXY_HOPS", 1, 0..=8)? as usize;
        let rate_token_ip_per_min =
            number("ASTER_SRV_RATE_TOKEN_IP_PER_MIN", 10, 1..=100_000)? as u32;
        let rate_api_ip_per_min =
            number("ASTER_SRV_RATE_API_IP_PER_MIN", 1200, 1..=1_000_000)? as u32;
        let rate_api_subject_per_min =
            number("ASTER_SRV_RATE_API_SUBJECT_PER_MIN", 3000, 1..=1_000_000)? as u32;
        let max_connections = number("ASTER_SRV_MAX_CONNECTIONS", 1024, 1..=1_000_000)? as usize;
        let max_large_bodies = number("ASTER_SRV_MAX_LARGE_BODIES", 8, 1..=1024)? as usize;
        let db_wait = Duration::from_secs(number("ASTER_SRV_DB_WAIT_SECS", 5, 1..=300)?);
        let header_timeout =
            Duration::from_secs(number("ASTER_SRV_HEADER_TIMEOUT_SECS", 20, 1..=300)?);
        let body_timeout =
            Duration::from_secs(number("ASTER_SRV_BODY_TIMEOUT_SECS", 60, 1..=3600)?);
        let unauth_body_timeout =
            Duration::from_secs(number("ASTER_SRV_UNAUTH_BODY_TIMEOUT_SECS", 8, 1..=60)?)
                .min(body_timeout);
        let max_connections_per_ip =
            number("ASTER_SRV_MAX_CONNECTIONS_PER_IP", 128, 1..=1_000_000)? as usize;
        let handler_timeout =
            Duration::from_secs(number("ASTER_SRV_HANDLER_TIMEOUT_SECS", 120, 1..=3600)?);
        // Sign-in hands out tokens to whoever IAP names, so it needs IAP to be
        // real and the dashboard's domain allowlist to be set. Without them a
        // development viewer (no IAP) or any Google identity would get one.
        if !cli_token_keys.is_empty() {
            if iap_audience.is_none() {
                bail!(
                    "ASTER_SRV_CLI_TOKEN_KEYS needs ASTER_SRV_IAP_AUDIENCE: person tokens are only \
                     issued to identities IAP verified"
                );
            }
            if ui_email_domains.is_none() {
                bail!(
                    "ASTER_SRV_CLI_TOKEN_KEYS needs ASTER_SRV_UI_EMAIL_DOMAINS: without it any \
                     identity that passes IAP gets a token"
                );
            }
            if var("ASTER_SRV_UI_DEV_EMAIL").is_some() {
                bail!(
                    "ASTER_SRV_UI_DEV_EMAIL must not be set together with ASTER_SRV_CLI_TOKEN_KEYS"
                );
            }
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
            default_branch,
            broken_runs,
            max_body_bytes,
            max_large_bodies,
            iap_audience,
            ui_email_domains,
            ui_dev_email: var("ASTER_SRV_UI_DEV_EMAIL").map(|email| email.trim().to_string()),
            quarantine,
            webhooks,
            cache_bucket,
            cache_signer,
            cache_days,
            cache_writers,
            writer_workflows,
            owner_ids,
            dev_blob_route: var("ASTER_SRV_DEV_BLOB_ROUTE").as_deref() == Some("1"),
            public_url,
            cli_token_keys,
            cli_token_ttl,
            api_listen,
            trusted_proxy_hops,
            rate_token_ip_per_min,
            rate_api_ip_per_min,
            rate_api_subject_per_min,
            body_timeout,
            unauth_body_timeout,
            max_connections_per_ip,
            handler_timeout,
            header_timeout,
            max_connections,
            db_wait,
            person_tokens_enabled,
        })
    }
}

/// The least key material HS256 should have: the size of its hash (RFC 7518
/// section 3.2 requires a key at least as long as the hash output).
pub const MIN_KEY_BYTES: usize = 32;

/// Decode a key secret to its bytes and check it carries 256 bits.
///
/// A secret is written as hex (64 or more digits) or base64 / base64url (43
/// or more characters), such as the output of `openssl rand -base64 32` or
/// `openssl rand -hex 32`. A passphrase is refused: its characters carry far
/// fewer than 8 bits each, so its length proves nothing about its strength.
/// As a guard against a pasted placeholder, the decoded bytes must also hold
/// at least 16 distinct values (random 32 bytes hold about 28).
pub fn decode_key_secret(secret: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let hex = secret.len() & 1 == 0 && secret.bytes().all(|b| b.is_ascii_hexdigit());
    let bytes = if hex {
        (0..secret.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&secret[i..i + 2], 16).expect("hex digits checked"))
            .collect::<Vec<u8>>()
    } else {
        let trimmed = secret.trim_end_matches('=');
        [
            base64::engine::general_purpose::STANDARD_NO_PAD,
            base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ]
        .iter()
        .find_map(|engine| engine.decode(trimmed).ok())
        .context(
            "ASTER_SRV_CLI_TOKEN_KEYS secrets must be hex or base64, such as `openssl rand -base64 32`",
        )?
    };
    if bytes.len() < MIN_KEY_BYTES {
        bail!(
            "ASTER_SRV_CLI_TOKEN_KEYS secrets must decode to at least {MIN_KEY_BYTES} bytes (256 bits); \
             this one decodes to {}",
            bytes.len()
        );
    }
    let distinct = bytes
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    // A short block repeated (`abababab...`, `0123456789abcdef` twice)
    // has the length of a key and the strength of its block.
    let repeats = (1..=bytes.len() / 2)
        .filter(|period| bytes.len() % period == 0 || *period <= 16)
        .any(|period| {
            bytes
                .iter()
                .enumerate()
                .all(|(i, b)| *b == bytes[i % period])
        });
    // A run that steps by a constant (`01 02 03 ...`) is as guessable.
    let stepped = bytes
        .windows(2)
        .all(|pair| pair[1].wrapping_sub(pair[0]) == bytes[1].wrapping_sub(bytes[0]));
    if distinct < 16 || repeats || stepped {
        bail!("ASTER_SRV_CLI_TOKEN_KEYS secret looks like a placeholder, not random bytes");
    }
    Ok(bytes)
}

/// `kid:secret` pairs. A kid is letters, digits, `-`, `_` or `.`; a secret
/// is hex or base64 for at least 32 random bytes ([`decode_key_secret`]).
fn parse_cli_token_keys(value: &str) -> Result<Vec<CliTokenKey>> {
    let mut keys: Vec<CliTokenKey> = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (kid, secret) = entry
            .split_once(':')
            .context("ASTER_SRV_CLI_TOKEN_KEYS entries are kid:secret")?;
        let (kid, secret) = (kid.trim(), secret.trim());
        if kid.is_empty()
            || !kid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            bail!("ASTER_SRV_CLI_TOKEN_KEYS key ids use letters, digits, '-', '_' and '.'");
        }
        let secret = decode_key_secret(secret)?;
        if keys.iter().any(|key| key.kid == kid) {
            bail!("ASTER_SRV_CLI_TOKEN_KEYS names key id '{kid}' twice");
        }
        keys.push(CliTokenKey {
            kid: kid.to_string(),
            secret,
        });
    }
    if keys.is_empty() {
        bail!("ASTER_SRV_CLI_TOKEN_KEYS lists no key");
    }
    Ok(keys)
}

/// Prefixes of the `job_workflow_ref` claim that may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterWorkflows(Vec<String>);

impl WriterWorkflows {
    pub fn parse(value: &str) -> Result<Self> {
        let prefixes: Vec<String> = value
            .split(',')
            .map(|prefix| prefix.trim().to_string())
            .filter(|prefix| !prefix.is_empty())
            .collect();
        if prefixes.is_empty() {
            bail!("ASTER_SRV_CACHE_WRITER_WORKFLOWS lists no workflow");
        }
        // Without an `@` the prefix `owner/repo/.github/workflows/ci.yml`
        // would also match `ci.yml.evil@...`.
        if let Some(bad) = prefixes.iter().find(|prefix| !prefix.contains('@')) {
            bail!(
                "ASTER_SRV_CACHE_WRITER_WORKFLOWS entry '{bad}' must end its workflow path with '@', \
                 such as owner/repo/.github/workflows/ci.yml@"
            );
        }
        Ok(Self(prefixes))
    }

    /// Whether `workflow` (a `job_workflow_ref`) starts with a prefix.
    pub fn allows(&self, workflow: &str) -> bool {
        self.0.iter().any(|prefix| workflow.starts_with(prefix))
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

/// `repository_owner_id` pins: `owner/name` or `owner/*` to a GitHub id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerIds(Vec<(String, String)>);

impl OwnerIds {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        let mut pins = Vec::new();
        for entry in value
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            let (repo, id) = entry.split_once('=').with_context(|| {
                format!("ASTER_SRV_REPOSITORY_OWNER_IDS entry '{entry}' must be repo=id")
            })?;
            let repo = repo.trim().to_ascii_lowercase();
            let id = id.trim();
            if repo
                .split_once('/')
                .is_none_or(|(o, n)| o.is_empty() || n.is_empty())
                || id.is_empty()
                || !id.bytes().all(|b| b.is_ascii_digit())
            {
                bail!("ASTER_SRV_REPOSITORY_OWNER_IDS entry '{entry}' must be owner/name=digits or owner/*=digits");
            }
            pins.push((repo, id.to_string()));
        }
        Ok(Self(pins))
    }

    /// Whether `repository` with this `repository_owner_id` claim is
    /// acceptable: no pin covers it, or the claim equals the pin.
    pub fn accepts(&self, repository: &str, claim: Option<&str>) -> bool {
        let repository = repository.to_ascii_lowercase();
        let owner = repository.split_once('/').map(|(owner, _)| owner);
        self.0
            .iter()
            .filter(|(pattern, _)| {
                *pattern == repository
                    || pattern.strip_suffix("/*").is_some_and(|o| Some(o) == owner)
            })
            .all(|(_, id)| claim == Some(id.as_str()))
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
                days: 30,
                run_cap: 10
            }
        );
        assert!(config.webhooks.is_empty());
        assert!(config.cache_bucket.is_none());
        assert!(config.cache_signer.is_none());
        assert_eq!(config.cache_days, 14);
        assert!(config
            .cache_writers
            .allows("ArchAstro/aster", Some("refs/heads/main")));
        assert!(!config
            .cache_writers
            .allows("ArchAstro/aster", Some("refs/pull/7/merge")));
        assert!(!config.cache_writers.allows("ArchAstro/aster", None));
        // Person sign-in and the workflow limit are off until configured.
        assert!(config.writer_workflows.is_none());
        assert!(config.public_url.is_none());
        assert!(config.cli_token_keys.is_empty());
        assert_eq!(config.cli_token_ttl, 12 * 60 * 60);
        assert!(config.person_tokens_enabled);
    }

    #[test]
    fn the_person_token_kill_switch_is_on_or_off() {
        let with = |value: &'static str| {
            Config::from_env(|name| match name {
                "ASTER_SRV_DATABASE_URL" => Some("postgres://x/y".into()),
                "ASTER_SRV_REPOSITORIES" => Some("a/*".into()),
                "ASTER_SRV_PERSON_TOKENS" => Some(value.into()),
                _ => None,
            })
        };
        assert!(!with("off").unwrap().person_tokens_enabled);
        assert!(!with(" OFF ").unwrap().person_tokens_enabled);
        assert!(with("on").unwrap().person_tokens_enabled);
        assert!(with("maybe").is_err());
    }

    #[test]
    fn reads_sign_in_and_writer_workflow_settings() {
        let with = |extra: &[(&str, &str)]| {
            let mut pairs = REQUIRED.to_vec();
            pairs.extend_from_slice(extra);
            Config::from_env(env_of(&pairs))
        };
        let secret = "q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=";
        let secret2 = "Zk3pQw9TnVb2Lx8RcJ5mYd7HsAe1UgF4oIiNtXuMvB0";
        let config = with(&[
            (
                "ASTER_SRV_IAP_AUDIENCE",
                "/projects/1/global/backendServices/4242",
            ),
            ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
            ("ASTER_SRV_PUBLIC_URL", "https://aster-srv.example.com/"),
            (
                "ASTER_SRV_CLI_TOKEN_KEYS",
                &format!("k2:{secret}, k1:{secret2}"),
            ),
            ("ASTER_SRV_CLI_TOKEN_TTL", "3600"),
            (
                "ASTER_SRV_CACHE_WRITER_WORKFLOWS",
                "A/b/.github/workflows/ci.yml@, A/c/.github/workflows/x.yml@refs/heads/main",
            ),
        ])
        .unwrap();
        assert_eq!(
            config.public_url.as_deref(),
            Some("https://aster-srv.example.com")
        );
        let kids: Vec<_> = config
            .cli_token_keys
            .iter()
            .map(|k| k.kid.as_str())
            .collect();
        assert_eq!(kids, ["k2", "k1"], "the first key signs");
        assert_eq!(config.cli_token_ttl, 3600);
        let workflows = config.writer_workflows.unwrap();
        assert!(workflows.allows("A/b/.github/workflows/ci.yml@refs/heads/main"));
        assert!(workflows.allows("A/c/.github/workflows/x.yml@refs/heads/main"));
        assert!(!workflows.allows("A/b/.github/workflows/ci.yml.evil@refs/heads/main"));
        assert!(!workflows.allows("A/c/.github/workflows/x.yml@refs/heads/dev"));
        // Secrets stay out of debug output.
        assert!(!format!("{:?}", config.cli_token_keys).contains(secret));
        assert_eq!(config.cli_token_keys[0].secret.len(), 32);

        for (name, value) in [
            ("ASTER_SRV_PUBLIC_URL", "aster-srv.example.com"),
            ("ASTER_SRV_PUBLIC_URL", "https://x.example.com/?a=b"),
            ("ASTER_SRV_CLI_TOKEN_KEYS", "k1:short"),
            ("ASTER_SRV_CLI_TOKEN_KEYS", "no-colon"),
            (
                "ASTER_SRV_CLI_TOKEN_KEYS",
                "bad kid:q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=",
            ),
            (
                "ASTER_SRV_CLI_TOKEN_KEYS",
                "k:q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=,k:Zk3pQw9TnVb2Lx8RcJ5mYd7HsAe1UgF4oIiNtXuMvB0",
            ),
            ("ASTER_SRV_CLI_TOKEN_TTL", "30"),
            ("ASTER_SRV_CLI_TOKEN_TTL", "999999999"),
            ("ASTER_SRV_CLI_TOKEN_TTL", "43201"),
            ("ASTER_SRV_CLI_TOKEN_TTL", "12h"),
            (
                "ASTER_SRV_CACHE_WRITER_WORKFLOWS",
                "A/b/.github/workflows/ci.yml",
            ),
        ] {
            assert!(with(&[(name, value)]).is_err(), "{name}={value}");
        }
    }

    const GOOD_KEY: &str = "q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=";
    const SIGN_IN: [(&str, &str); 4] = [
        ("ASTER_SRV_PUBLIC_URL", "https://aster-srv.example.com"),
        (
            "ASTER_SRV_IAP_AUDIENCE",
            "/projects/1/global/backendServices/4242",
        ),
        ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
        (
            "ASTER_SRV_CLI_TOKEN_KEYS",
            "k1:q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=",
        ),
    ];

    fn with_sign_in(drop: &[&str], extra: &[(&str, &str)]) -> Result<Config> {
        let mut pairs = REQUIRED.to_vec();
        pairs.extend(SIGN_IN.iter().filter(|(name, _)| !drop.contains(name)));
        pairs.extend_from_slice(extra);
        Config::from_env(env_of(&pairs))
    }

    #[test]
    fn token_keys_need_256_bits_of_hex_or_base64_that_look_random() {
        let hex64 = "9f3a1c7be24d805ae6c1b94f7d20a3e85c6b1f4d9a07e2c38b5d6f1a4c9e0b72";
        let bytes = decode_key_secret(hex64).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x9f);
        assert_eq!(decode_key_secret(GOOD_KEY).unwrap().len(), 32);
        // Unpadded and url-safe forms are the same key material.
        assert_eq!(
            decode_key_secret(GOOD_KEY.trim_end_matches('=')).unwrap(),
            decode_key_secret(GOOD_KEY).unwrap()
        );
        for (why, secret) in [
            ("a passphrase", "correct horse battery staple correct horse"),
            (
                "31 bytes of base64",
                "q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNt",
            ),
            ("hex for 31 bytes", &hex64[..62]),
            ("one repeated byte", &"ab".repeat(32)),
            ("a repeated block", &"0123456789abcdef".repeat(4)),
            (
                "counting up",
                &(0u8..32).map(|b| format!("{b:02x}")).collect::<String>(),
            ),
            ("all zeros", &"00".repeat(32)),
            (
                "the old placeholder",
                "test-signing-secret-0123456789abcdef",
            ),
            ("empty", ""),
        ] {
            assert!(decode_key_secret(secret).is_err(), "{why} was accepted");
        }
    }

    #[test]
    fn a_weak_key_stops_the_server_from_starting() {
        let err = with_sign_in(
            &["ASTER_SRV_CLI_TOKEN_KEYS"],
            &[(
                "ASTER_SRV_CLI_TOKEN_KEYS",
                "k1:0123456789abcdef0123456789abcdef",
            )],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("256 bits") || format!("{err:#}").contains("placeholder"),
            "{err:#}"
        );
    }

    #[test]
    fn a_public_url_must_be_https_unless_a_development_flag_says_otherwise() {
        let http = ("ASTER_SRV_PUBLIC_URL", "http://127.0.0.1:8080");
        let err = with_sign_in(&["ASTER_SRV_PUBLIC_URL"], &[http]).unwrap_err();
        assert!(format!("{err:#}").contains("https"), "{err:#}");
        let config = with_sign_in(
            &["ASTER_SRV_PUBLIC_URL"],
            &[http, ("ASTER_SRV_ALLOW_HTTP_PUBLIC_URL", "1")],
        )
        .unwrap();
        assert_eq!(config.public_url.as_deref(), Some("http://127.0.0.1:8080"));
        // Only the exact value `1` opts out.
        assert!(with_sign_in(
            &["ASTER_SRV_PUBLIC_URL"],
            &[http, ("ASTER_SRV_ALLOW_HTTP_PUBLIC_URL", "true")]
        )
        .is_err());
        assert!(with_sign_in(&[], &[]).is_ok());
    }

    #[test]
    fn sign_in_refuses_to_start_without_real_iap_a_domain_allowlist_and_no_dev_viewer() {
        // Dev viewer without IAP: anyone could mint a token as that email.
        let err = with_sign_in(
            &["ASTER_SRV_IAP_AUDIENCE"],
            &[("ASTER_SRV_UI_DEV_EMAIL", "me@example.com")],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("ASTER_SRV_IAP_AUDIENCE"),
            "{err:#}"
        );
        // No IAP at all.
        assert!(with_sign_in(&["ASTER_SRV_IAP_AUDIENCE"], &[]).is_err());
        // Any IAP identity would get a token.
        let err = with_sign_in(&["ASTER_SRV_UI_EMAIL_DOMAINS"], &[]).unwrap_err();
        assert!(
            format!("{err:#}").contains("ASTER_SRV_UI_EMAIL_DOMAINS"),
            "{err:#}"
        );
        // A dev viewer next to real IAP is ignored by the dashboard, but with
        // sign-in on it is a misconfiguration worth stopping for.
        let err = with_sign_in(&[], &[("ASTER_SRV_UI_DEV_EMAIL", "me@example.com")]).unwrap_err();
        assert!(
            format!("{err:#}").contains("ASTER_SRV_UI_DEV_EMAIL"),
            "{err:#}"
        );
        // Sign-in off: the dashboard's own modes are unchanged.
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_UI_DEV_EMAIL", "me@example.com"));
        assert!(Config::from_env(env_of(&pairs)).is_ok());
    }

    #[test]
    fn reads_the_listener_limit_and_timeout_settings() {
        let config = Config::from_env(env_of(&REQUIRED)).unwrap();
        assert_eq!(config.api_listen, None);
        assert_eq!(config.trusted_proxy_hops, 1);
        assert_eq!(config.rate_token_ip_per_min, 10);
        assert_eq!(config.rate_api_ip_per_min, 1200);
        assert_eq!(config.rate_api_subject_per_min, 3000);
        assert_eq!(config.body_timeout, Duration::from_secs(60));
        assert_eq!(config.handler_timeout, Duration::from_secs(120));
        assert_eq!(config.max_connections, 1024);
        assert_eq!(config.header_timeout, Duration::from_secs(20));
        assert_eq!(config.db_wait, Duration::from_secs(5));
        let with = |extra: &[(&str, &str)]| {
            let mut pairs = REQUIRED.to_vec();
            pairs.extend_from_slice(extra);
            Config::from_env(env_of(&pairs))
        };
        let config = with(&[
            ("ASTER_SRV_API_ADDR", "0.0.0.0:8081"),
            ("ASTER_SRV_TRUSTED_PROXY_HOPS", "2"),
            ("ASTER_SRV_RATE_TOKEN_IP_PER_MIN", "5"),
            ("ASTER_SRV_MAX_CONNECTIONS", "50"),
        ])
        .unwrap();
        assert_eq!(config.api_listen.unwrap().port(), 8081);
        assert_eq!(config.trusted_proxy_hops, 2);
        assert_eq!(config.rate_token_ip_per_min, 5);
        assert_eq!(config.max_connections, 50);
        for (name, bad) in [
            ("ASTER_SRV_API_ADDR", "8081"),
            ("ASTER_SRV_API_ADDR", "0.0.0.0:8080"),
            ("ASTER_SRV_TRUSTED_PROXY_HOPS", "9"),
            ("ASTER_SRV_RATE_TOKEN_IP_PER_MIN", "0"),
            ("ASTER_SRV_BODY_TIMEOUT_SECS", "0"),
            ("ASTER_SRV_HANDLER_TIMEOUT_SECS", "99999"),
            ("ASTER_SRV_MAX_CONNECTIONS", "0"),
            ("ASTER_SRV_DB_WAIT_SECS", "soon"),
        ] {
            assert!(with(&[(name, bad)]).is_err(), "{name}={bad}");
        }
    }

    #[test]
    fn reads_the_quarantine_rules() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_QUARANTINE_RUNS", "0"));
        pairs.push(("ASTER_SRV_QUARANTINE_RELEASE_PASSES", "5"));
        pairs.push(("ASTER_SRV_QUARANTINE_DAYS", "14"));
        pairs.push(("ASTER_SRV_QUARANTINE_RUN_CAP", "3"));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(
            config.quarantine,
            QuarantineRules {
                runs: 0,
                release_passes: 5,
                days: 14,
                run_cap: 3
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
            ("ASTER_SRV_QUARANTINE_RUN_CAP", "0"),
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
            "/projects/123456/global/backendServices/987",
        ));
        pairs.push(("ASTER_SRV_UI_EMAIL_DOMAINS", "Example.com, *.corp.example"));
        pairs.push(("ASTER_SRV_UI_DEV_EMAIL", "me@example.com"));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(
            config.iap_audience.as_deref(),
            Some("/projects/123456/global/backendServices/987")
        );
        let domains = config.ui_email_domains.unwrap();
        assert!(domains.allows("a@example.com"));
        assert_eq!(config.ui_dev_email.as_deref(), Some("me@example.com"));

        for (name, bad) in [
            ("ASTER_SRV_IAP_AUDIENCE", "no-slash"),
            // The project-wide prefix would accept another backend's assertion.
            (
                "ASTER_SRV_IAP_AUDIENCE",
                "/projects/123456/global/backendServices/",
            ),
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
    fn writers_need_workflows_and_owner_ids_pin_repositories() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_CACHE_WRITERS", "*=refs/heads/main"));
        assert!(Config::from_env(env_of(&pairs)).is_err());
        pairs.push(("ASTER_SRV_ALLOW_ANY_WRITER_WORKFLOW", "1"));
        assert!(Config::from_env(env_of(&pairs)).is_ok());
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_CACHE_WRITERS", "*=refs/heads/main"));
        pairs.push((
            "ASTER_SRV_CACHE_WRITER_WORKFLOWS",
            "ArchAstro/firstlanding/.github/workflows/ci.yml@",
        ));
        assert!(Config::from_env(env_of(&pairs)).is_ok());

        let pins = OwnerIds::parse(Some("ArchAstro/*=123, other/x=9")).unwrap();
        assert!(pins.accepts("archastro/firstlanding", Some("123")));
        assert!(!pins.accepts("ArchAstro/firstlanding", Some("124")));
        assert!(!pins.accepts("ArchAstro/firstlanding", None));
        assert!(pins.accepts("unpinned/repo", None));
        for bad in ["a/b", "a/b=x", "ab=1", "=1", "a/*="] {
            assert!(OwnerIds::parse(Some(bad)).is_err(), "{bad}");
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
    fn a_cache_bucket_needs_a_signer() {
        let mut pairs = REQUIRED.to_vec();
        pairs.push(("ASTER_SRV_CACHE_BUCKET", "aster-cache"));
        let error = Config::from_env(env_of(&pairs)).unwrap_err();
        assert!(error.to_string().contains("ASTER_SRV_CACHE_SIGNER"));
        pairs.push(("ASTER_SRV_CACHE_SIGNER", "srv@proj.iam.gserviceaccount.com"));
        let config = Config::from_env(env_of(&pairs)).unwrap();
        assert_eq!(
            config.cache_signer.as_deref(),
            Some("srv@proj.iam.gserviceaccount.com")
        );
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
