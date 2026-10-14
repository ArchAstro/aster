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
    let listen = config.listen;
    let app = App::start(config).await?;
    aster_srv::test_results::retention::spawn(app.clone());
    aster_srv::test_results::classifier::spawn(app.clone());
    let listener = tokio::net::TcpListener::bind(listen).await?;
    log::info(
        "aster-srv listening",
        json!({"address": listen.to_string()}),
    );
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
