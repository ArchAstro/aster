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
pub mod log;
pub mod raw_store;
pub mod test_results;

/// An HTTP client builder on aws-lc-rs, the workspace's one TLS provider.
/// reqwest is built without its own (it would bring ring, and two
/// providers leave rustls with no default).
pub fn http_client() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    reqwest::Client::builder()
}

use std::sync::Arc;

use anyhow::Result;

pub use config::Config;
pub use http::{serve, Reply};

/// Everything a request handler needs.
pub struct App {
    pub config: Config,
    pub pool: deadpool_postgres::Pool,
    pub verifier: auth::Verifier,
    pub raw_store: raw_store::RawStore,
}

/// Every feature's migrations, in order.
pub fn migrations() -> Vec<&'static db::Migration> {
    test_results::MIGRATIONS.iter().collect()
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
        let pool = db::pool(&config.database_url)?;
        db::migrate(&pool, &migrations()).await?;
        let raw_store =
            raw_store.unwrap_or_else(|| raw_store::RawStore::new(config.raw_bucket.clone()));
        Ok(Arc::new(Self {
            config,
            pool,
            verifier,
            raw_store,
        }))
    }
}
