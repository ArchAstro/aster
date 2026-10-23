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
    let api_listen = config.api_listen;
    let app = App::connect(config)?;
    aster_srv::audit::Sampler::spawn_flush(&app.audit_sampler);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    log::info(
        "aster-srv listening",
        json!({"address": listen.to_string()}),
    );
    // Migrate in the background so `/healthz` answers meanwhile; see
    // `App::migrate_until_ready`.
    tokio::spawn({
        let app = app.clone();
        async move {
            let on_ready_app = app.clone();
            app.migrate_until_ready(
                &aster_srv::RetryPolicy::default(),
                |severity, message, fields| match severity {
                    aster_srv::Severity::Warning => log::warn(message, fields),
                    aster_srv::Severity::Error => log::error(message, fields),
                },
                move || {
                    aster_srv::test_results::retention::spawn(on_ready_app.clone());
                    aster_srv::cache::spawn(on_ready_app.clone());
                    aster_srv::test_results::classifier::spawn(on_ready_app.clone());
                    aster_srv::webhooks::spawn(on_ready_app);
                },
            )
            .await;
        }
    });
    // One shutdown signal stops both listeners.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let wait = |mut stopped: tokio::sync::watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };
    let api = match api_listen {
        Some(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            log::info(
                "aster-srv serving /v1 only",
                json!({"address": addr.to_string()}),
            );
            Some(tokio::spawn(aster_srv::serve_api(
                app.clone(),
                listener,
                wait(stopped.clone()),
            )))
        }
        None => None,
    };
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    });
    aster_srv::serve(app.clone(), listener, wait(stopped)).await?;
    if let Some(api) = api {
        api.await??;
    }
    app.audit_sampler.flush_all();
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
