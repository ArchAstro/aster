//! aster-srv: Aster's server.
//!
//! One service for what Aster runs need beyond a single machine. Its first
//! feature is CI test results (`/v1/test-results/...`, the `test_results`
//! schema); others, such as a shared build cache, sit beside it under their
//! own routes and schema and share the core: configuration, GitHub Actions
//! OIDC authentication, migrations and the HTTP server.

pub mod auth;
pub mod config;
pub mod db;
mod http;
pub mod iap;
pub mod log;
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
pub use http::{serve, Body, Reply};

/// Everything a request handler needs.
pub struct App {
    pub config: Config,
    pub pool: deadpool_postgres::Pool,
    pub verifier: auth::Verifier,
    pub iap: iap::IapVerifier,
    pub raw_store: raw_store::RawStore,
    /// Test durations already answered (they cannot change).
    pub durations: test_results::durations::Answers,
    /// Set once every migration has been applied. Until then the server
    /// answers `/healthz` and nothing else (see [`App::connect`]).
    schema_ready: AtomicBool,
}

/// Every feature's migrations, in order.
pub fn migrations() -> Vec<&'static db::Migration> {
    test_results::MIGRATIONS
        .iter()
        .chain(webhooks::MIGRATIONS)
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
        let app = Self::connect_with_iap(config, verifier, iap, raw_store)?;
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
        Self::connect_with_iap(config, verifier, iap, None)
    }

    fn connect_with_iap(
        config: Config,
        verifier: auth::Verifier,
        iap: iap::IapVerifier,
        raw_store: Option<raw_store::RawStore>,
    ) -> Result<Arc<Self>> {
        let pool = db::pool(&config.database_url)?;
        let raw_store =
            raw_store.unwrap_or_else(|| raw_store::RawStore::new(config.raw_bucket.clone()));
        Ok(Arc::new(Self {
            config,
            pool,
            verifier,
            iap,
            raw_store,
            durations: Default::default(),
            schema_ready: AtomicBool::new(false),
        }))
    }

    /// Apply every migration not yet applied, then mark the schema ready.
    pub async fn migrate(&self) -> Result<()> {
        db::migrate(&self.pool, &migrations()).await?;
        self.schema_ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Whether every migration has been applied.
    pub fn schema_ready(&self) -> bool {
        self.schema_ready.load(Ordering::Acquire)
    }
}
