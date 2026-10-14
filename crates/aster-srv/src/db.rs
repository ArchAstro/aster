//! Postgres: the connection pool and schema migrations.
//!
//! Each feature owns its migrations, numbered from 1 and applied in order
//! once. Replicas starting together take a session advisory lock so only
//! one applies them.

use std::sync::Arc;

use anyhow::{Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};

/// One schema change.
pub struct Migration {
    /// The feature it belongs to, e.g. `test_results`.
    pub feature: &'static str,
    pub version: i32,
    pub sql: &'static str,
}

/// A pool for `url`.
///
/// The URL's `sslmode` decides TLS: `disable` connects in plain TCP,
/// `prefer` (the default) tries TLS first, and `require` insists on it.
/// As with libpq's `require`, the connection is encrypted but the server's
/// certificate is not verified: Cloud SQL's private-IP certificates do not
/// name the address the server dials.
pub fn pool(url: &str) -> Result<Pool> {
    let config: tokio_postgres::Config = url.parse().context("cannot read the database URL")?;
    let manager_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let manager = if config.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
        Manager::from_config(config, tokio_postgres::NoTls, manager_config)
    } else {
        Manager::from_config(config, encrypting_connector()?, manager_config)
    };
    Pool::builder(manager)
        .max_size(16)
        .build()
        .context("cannot build the database pool")
}

fn encrypting_connector() -> Result<tokio_postgres_rustls::MakeRustlsConnect> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .context("cannot configure TLS")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(EncryptOnly { provider }))
        .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

/// Accepts any server certificate but still checks the handshake
/// signatures, which is what `sslmode=require` promises: encryption, not
/// identity.
#[derive(Debug)]
struct EncryptOnly {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

const MIGRATION_LOCK: i64 = 0x6173_7465_722d_7372; // "aster-sr"
/// How long a migration waits for a table lock before it gives up and
/// tries again.
const MIGRATION_LOCK_TIMEOUT: &str = "5s";
/// Attempts at a migration that keeps meeting a deadlock or a lock timeout.
const MIGRATION_ATTEMPTS: u32 = 12;

/// Apply every migration not yet applied.
pub async fn migrate(pool: &Pool, migrations: &[&Migration]) -> Result<()> {
    let mut client = pool.get().await.context("cannot connect to the database")?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS aster_srv_migrations (
                feature text NOT NULL,
                version integer NOT NULL,
                applied_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (feature, version)
            )",
        )
        .await?;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK])
        .await?;
    let result = async {
        for migration in migrations {
            let applied = client
                .query_opt(
                    "SELECT 1 FROM aster_srv_migrations WHERE feature = $1 AND version = $2",
                    &[&migration.feature, &migration.version],
                )
                .await?
                .is_some();
            if applied {
                continue;
            }
            apply(&mut client, migration).await?;
            crate::log::info(
                "applied migration",
                serde_json::json!({"feature": migration.feature, "version": migration.version}),
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK])
        .await?;
    result
}

/// Apply one migration in its own transaction.
///
/// During a rolling deploy the previous release keeps writing to the tables
/// a migration alters, so the migration can deadlock with it or wait behind
/// it. Such a migration rolls back and is tried again after a pause rather
/// than failing the server's start.
async fn apply(client: &mut deadpool_postgres::Client, migration: &Migration) -> Result<()> {
    let mut attempt = 1;
    loop {
        let transaction = client.transaction().await?;
        let applied = async {
            transaction
                .batch_execute(&format!(
                    "SET LOCAL lock_timeout = '{MIGRATION_LOCK_TIMEOUT}'"
                ))
                .await?;
            transaction.batch_execute(migration.sql).await?;
            transaction
                .execute(
                    "INSERT INTO aster_srv_migrations (feature, version) VALUES ($1, $2)",
                    &[&migration.feature, &migration.version],
                )
                .await?;
            transaction.commit().await
        }
        .await;
        let error = match applied {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let contended = error.code().is_some_and(|code| {
            *code == tokio_postgres::error::SqlState::T_R_DEADLOCK_DETECTED
                || *code == tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE
        });
        if !contended || attempt >= MIGRATION_ATTEMPTS {
            return Err(anyhow::Error::new(error).context(format!(
                "migration {} {} failed",
                migration.feature, migration.version
            )));
        }
        crate::log::warn(
            "migration waiting for table locks",
            serde_json::json!({
                "feature": migration.feature,
                "version": migration.version,
                "attempt": attempt,
                "error": error.to_string(),
            }),
        );
        tokio::time::sleep(std::time::Duration::from_secs(u64::from(attempt.min(5)))).await;
        attempt += 1;
    }
}
