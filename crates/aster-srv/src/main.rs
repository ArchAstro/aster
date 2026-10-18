use std::process::ExitCode;

use aster_srv::{log, App, Config};
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error("aster-srv stopped", json!({"error": format!("{error:#}")}));
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    // Name the process's TLS provider: rustls is built with more than one.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let config = Config::from_env(|name| std::env::var(name).ok())?;
    if !config.static_tokens.is_empty() {
        log::warn(
            "static tokens are enabled; use them only for development",
            json!({"repositories": config.static_tokens.iter().map(|t| &t.repository).collect::<Vec<_>>()}),
        );
    }
    if config.iap_audience.is_none() {
        if let Some(email) = &config.ui_dev_email {
            log::warn(
                "the dashboard trusts ASTER_SRV_UI_DEV_EMAIL without IAP; use it only for development",
                json!({"viewer": email}),
            );
        }
    } else if config.ui_dev_email.is_some() {
        log::warn(
            "ASTER_SRV_UI_DEV_EMAIL is ignored because ASTER_SRV_IAP_AUDIENCE is set",
            json!({}),
        );
    }
    let listen = config.listen;
    let app = App::connect(config)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    log::info(
        "aster-srv listening",
        json!({"address": listen.to_string()}),
    );
    // Migrate in the background so `/healthz` answers meanwhile. A rolling
    // deploy can hold table locks for minutes; exiting after a few attempts
    // (or being killed by the liveness probe) only restarts the wait.
    tokio::spawn({
        let app = app.clone();
        async move {
            let mut attempt = 1u32;
            while let Err(error) = app.migrate().await {
                log::warn(
                    "migrations not applied yet; retrying",
                    json!({"attempt": attempt, "error": format!("{error:#}")}),
                );
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            aster_srv::test_results::retention::spawn(app.clone());
            aster_srv::test_results::classifier::spawn(app.clone());
            aster_srv::webhooks::spawn(app);
        }
    });
    aster_srv::serve(app, listener, shutdown_signal()).await?;
    log::info("aster-srv stopped", json!({}));
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler installs");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
