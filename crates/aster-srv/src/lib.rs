//! aster-srv: Aster's server.
//!
//! One service for what Aster runs need beyond a single machine. Its first
//! feature is CI test results (`/v1/test-results/...`, the `test_results`
//! schema); the shared artifact cache (`/v1/cache/...`, the `cache` schema)
//! sits beside it. Both share the core: configuration, GitHub Actions
//! OIDC authentication, migrations and the HTTP server. People sign in
//! from a laptop with `aster cache login` (`/cli-auth`, [`cli_auth`]) and
//! hold a token that reads the cache ([`person`]).

pub mod audit;
pub mod auth;
pub mod cache;
pub mod cli_auth;
pub mod config;
pub mod db;
mod http;
pub mod iap;
pub mod limits;
pub mod log;
pub mod metrics;
pub mod person;
pub mod raw_store;
pub mod test_results;
mod ui;
pub mod webhooks;

/// An HTTP client builder on aws-lc-rs, the workspace's one TLS provider.
/// reqwest is built without its own (it would bring ring, and two
/// providers leave rustls with no default).
pub fn http_client() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    reqwest::Client::builder()
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

pub use config::Config;
pub use http::{serve, serve_api, Body, Reply, Surface};

/// How [`App::migrate_until_ready`] paces and escalates its retries.
pub struct RetryPolicy {
    /// Pause after the first failed pass.
    pub first_pause: std::time::Duration,
    /// Pause after every later failed pass.
    pub later_pause: std::time::Duration,
    /// How long a table-lock wait stays a warning before it is an error.
    pub escalate_after: std::time::Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            first_pause: std::time::Duration::from_secs(5),
            later_pause: std::time::Duration::from_secs(30),
            escalate_after: std::time::Duration::from_secs(600),
        }
    }
}

/// Log severity for [`App::migrate_until_ready`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

/// Everything a request handler needs.
pub struct App {
    pub config: Config,
    pub pool: deadpool_postgres::Pool,
    /// A small pool only `POST /v1/auth/token` uses. That route is
    /// unauthenticated, so a flood of it can exhaust this pool and nothing
    /// else: CI commits and ingest keep [`Self::pool`].
    pub auth_pool: deadpool_postgres::Pool,
    pub limits: limits::Limits,
    pub metrics: Arc<metrics::Metrics>,
    pub verifier: auth::Verifier,
    pub iap: iap::IapVerifier,
    /// Mints and checks person tokens. `None` when
    /// `ASTER_SRV_CLI_TOKEN_KEYS` or `ASTER_SRV_PUBLIC_URL` is unset: sign-in
    /// is then off and person tokens are refused.
    pub person_tokens: Option<person::TokenIssuer>,
    pub raw_store: raw_store::RawStore,
    /// Blobs for the shared artifact cache. Disabled when no bucket is set.
    pub cache_store: cache::BlobStore,
    /// Test durations already answered (they cannot change).
    pub durations: test_results::durations::Answers,
    /// Set once every migration has been applied. Until then the server
    /// answers `/healthz` and nothing else (see [`App::connect`]).
    schema_ready: AtomicBool,
    /// The last failed migration pass, for `/readyz` while not ready.
    migration_failure: std::sync::Mutex<Option<MigrationFailure>>,
}

/// Why the schema is not ready yet, as `/readyz` reports it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MigrationFailure {
    /// `waiting_for_locks` (clears once the other release lets go) or
    /// `failing` (waiting will not fix it; an operator must).
    pub state: &'static str,
    /// Failed passes so far.
    pub pass: u32,
    /// Seconds since the first pass started.
    pub waiting_seconds: u64,
    /// The last pass's error, for logs only. Not serialized: `/readyz` is
    /// unauthenticated, and database errors can carry connection and schema
    /// details.
    #[serde(skip)]
    pub error: String,
}

/// Every feature's migrations, in order.
pub fn migrations() -> Vec<&'static db::Migration> {
    test_results::MIGRATIONS
        .iter()
        .chain(webhooks::MIGRATIONS)
        .chain(cache::MIGRATIONS)
        .chain(cli_auth::MIGRATIONS)
        .collect()
}

impl App {
    /// Connect to the database and bring its schema up to date.
    pub async fn start(config: Config) -> Result<Arc<Self>> {
        let verifier = auth::Verifier::new(&config);
        Self::start_with(config, verifier, None).await
    }

    /// [`Self::start`] with a chosen verifier and raw store. For tests.
    pub async fn start_with(
        config: Config,
        verifier: auth::Verifier,
        raw_store: Option<raw_store::RawStore>,
    ) -> Result<Arc<Self>> {
        let iap = iap::IapVerifier::new(&config);
        Self::start_with_iap(config, verifier, iap, raw_store).await
    }

    /// [`Self::start_with`] with a chosen dashboard verifier. For tests.
    pub async fn start_with_iap(
        config: Config,
        verifier: auth::Verifier,
        iap: iap::IapVerifier,
        raw_store: Option<raw_store::RawStore>,
    ) -> Result<Arc<Self>> {
        Self::start_with_cache(config, verifier, iap, raw_store, None).await
    }

    /// [`Self::start_with_iap`] with a chosen cache blob store. For tests.
    pub async fn start_with_cache(
        config: Config,
        verifier: auth::Verifier,
        iap: iap::IapVerifier,
        raw_store: Option<raw_store::RawStore>,
        cache_store: Option<cache::BlobStore>,
    ) -> Result<Arc<Self>> {
        let app = Self::connect_with_iap(config, verifier, iap, raw_store, cache_store)?;
        app.migrate().await?;
        Ok(app)
    }

    /// Build the app without touching the database's schema.
    ///
    /// The server can then listen and answer `/healthz` while [`Self::migrate`]
    /// waits for table locks, which a rolling deploy can hold for minutes
    /// (the previous release is still running its background passes). Every
    /// other route answers 503 until the migrations have been applied.
    pub fn connect(config: Config) -> Result<Arc<Self>> {
        let verifier = auth::Verifier::new(&config);
        let iap = iap::IapVerifier::new(&config);
        Self::connect_with_iap(config, verifier, iap, None, None)
    }

    fn connect_with_iap(
        config: Config,
        verifier: auth::Verifier,
        iap: iap::IapVerifier,
        raw_store: Option<raw_store::RawStore>,
        cache_store: Option<cache::BlobStore>,
    ) -> Result<Arc<Self>> {
        let pool = db::pool_sized(&config.database_url, 16, config.db_wait)?;
        let auth_pool =
            db::pool_sized(&config.database_url, limits::AUTH_POOL_SIZE, config.db_wait)?;
        let limits = limits::Limits::new(&config);
        let person_tokens = person::TokenIssuer::from_config(&config);
        let raw_store =
            raw_store.unwrap_or_else(|| raw_store::RawStore::new(config.raw_bucket.clone()));
        let cache_store = cache_store.unwrap_or_else(|| {
            cache::BlobStore::new(config.cache_bucket.clone(), config.cache_signer.clone())
        });
        Ok(Arc::new(Self {
            config,
            pool,
            auth_pool,
            limits,
            metrics: Default::default(),
            verifier,
            iap,
            person_tokens,
            raw_store,
            cache_store,
            durations: Default::default(),
            schema_ready: AtomicBool::new(false),
            migration_failure: Default::default(),
        }))
    }

    /// Who a bearer token names: a CI job or a signed-in person.
    pub async fn principal(
        &self,
        authorization: Option<&str>,
    ) -> Result<auth::Principal, auth::AuthError> {
        auth::authenticate(&self.verifier, self.person_tokens.as_ref(), authorization).await
    }

    /// Apply every migration not yet applied, then mark the schema ready.
    pub async fn migrate(&self) -> Result<()> {
        db::migrate(&self.pool, &migrations()).await?;
        self.schema_ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Migrate, retrying until it succeeds, then call `on_ready`.
    ///
    /// Never returns an error: a rolling deploy can hold table locks for
    /// minutes, and exiting only restarts the wait. Each pass is itself up to
    /// 12 attempts of `lock_timeout` 5s against the other release's readers,
    /// so the pause after the first pass is the longer `policy.later_pause`
    /// to keep the lock queue from stalling them. A table-lock wait logs a
    /// warning until it has lasted `policy.escalate_after`; any other error
    /// logs at error severity immediately. `on_ready` is where the background
    /// loops start: they must not run against an unmigrated schema.
    pub async fn migrate_until_ready(
        &self,
        policy: &RetryPolicy,
        mut log: impl FnMut(Severity, &str, serde_json::Value),
        on_ready: impl FnOnce(),
    ) {
        let started = std::time::Instant::now();
        let mut pass = 1u32;
        while let Err(error) = self.migrate().await {
            let contended = db::is_lock_contention(&error);
            let severity = if contended && started.elapsed() < policy.escalate_after {
                Severity::Warning
            } else {
                Severity::Error
            };
            let failure = MigrationFailure {
                state: if contended {
                    "waiting_for_locks"
                } else {
                    "failing"
                },
                pass,
                waiting_seconds: started.elapsed().as_secs(),
                error: format!("{error:#}"),
            };
            log(
                severity,
                if contended {
                    "migrations waiting for table locks; retrying"
                } else {
                    "migrations failed; retrying"
                },
                serde_json::json!({
                    "pass": failure.pass,
                    "waiting_seconds": failure.waiting_seconds,
                    "error": failure.error,
                }),
            );
            *self.migration_failure.lock().unwrap() = Some(failure);
            let pause = if pass == 1 {
                policy.first_pause
            } else {
                policy.later_pause
            };
            tokio::time::sleep(pause).await;
            pass += 1;
        }
        *self.migration_failure.lock().unwrap() = None;
        on_ready();
    }

    /// The last failed migration pass while the schema is not ready.
    ///
    /// Retries never stop, so a permanent failure leaves the process live
    /// (`/healthz` 200) and unready forever; this is how `/readyz` and
    /// rollout monitoring tell that apart from a lock wait that will clear.
    pub fn migration_failure(&self) -> Option<MigrationFailure> {
        self.migration_failure.lock().unwrap().clone()
    }

    /// Whether every migration has been applied.
    pub fn schema_ready(&self) -> bool {
        self.schema_ready.load(Ordering::Acquire)
    }
}
