//! The HTTP server and its routes.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::json;
use tokio::net::TcpListener;

use crate::auth::AuthError;
use crate::test_results::ingest::{self, Upload};
use crate::App;

/// A response before it is sent.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: serde_json::Value,
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self { status, body }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({"error": message.into()}))
    }

    fn into_response(self) -> Response<Full<Bytes>> {
        let mut response = Response::new(Full::new(Bytes::from(self.body.to_string())));
        *response.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response
    }
}

/// Serve `app` on `listener` until `shutdown` resolves, then give open
/// requests up to 30 seconds to finish.
pub async fn serve(
    app: Arc<App>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        crate::log::warn("accept failed", json!({"error": error.to_string()}));
                        continue;
                    }
                };
                let app = Arc::clone(&app);
                let service = service_fn(move |request| {
                    let app = Arc::clone(&app);
                    async move { Ok::<_, Infallible>(route(&app, request).await.into_response()) }
                });
                let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
                let connection = graceful.watch(connection);
                tokio::spawn(async move {
                    let _ = connection.await;
                });
            }
            _ = &mut shutdown => break,
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(30), graceful.shutdown()).await;
    Ok(())
}

async fn route(app: &App, request: Request<Incoming>) -> Reply {
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/healthz") => Reply::json(200, json!({"ok": true})),
        (&Method::GET, "/readyz") => match ready(app).await {
            Ok(()) => Reply::json(200, json!({"ok": true})),
            Err(error) => Reply::error(503, format!("the database is unavailable: {error}")),
        },
        (&Method::POST, "/v1/test-results/runs") => {
            let authorization = header(&request, AUTHORIZATION);
            let identity = match app.verifier.verify(authorization.as_deref()).await {
                Ok(identity) => identity,
                Err(error @ AuthError::Forbidden(_)) => {
                    return Reply::error(403, error.to_string())
                }
                Err(error) => return Reply::error(401, error.to_string()),
            };
            let gzip =
                header(&request, CONTENT_ENCODING).is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
            let body = match read_body(request, app.config.max_body_bytes).await {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            ingest::handle(app, &identity, Upload { body, gzip }).await
        }
        (_, "/healthz" | "/readyz" | "/v1/test-results/runs") => {
            Reply::error(405, "method not allowed")
        }
        _ => Reply::error(404, "not found"),
    }
}

fn header(request: &Request<Incoming>, name: hyper::header::HeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

async fn read_body(request: Request<Incoming>, limit: usize) -> Result<Bytes, Reply> {
    match Limited::new(request.into_body(), limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => Err(Reply::error(
            413,
            format!("the body is larger than {limit} bytes"),
        )),
        Err(error) => Err(Reply::error(400, format!("cannot read the body: {error}"))),
    }
}

async fn ready(app: &App) -> anyhow::Result<()> {
    let client = app.pool.get().await?;
    client.simple_query("SELECT 1").await?;
    Ok(())
}
