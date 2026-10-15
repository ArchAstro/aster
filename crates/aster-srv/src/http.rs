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

use crate::auth::{AuthError, Identity};
use crate::test_results::ingest::{self, Upload};
use crate::test_results::query;
use crate::App;

/// A response before it is sent.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Body,
}

/// What a reply carries.
#[derive(Debug)]
pub enum Body {
    Json(serde_json::Value),
    /// A page for a browser, sent with the dashboard's security headers.
    Html(String),
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: Body::Json(body),
        }
    }

    pub fn html(status: u16, page: String) -> Self {
        Self {
            status,
            body: Body::Html(page),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({"error": message.into()}))
    }

    fn into_response(self) -> Response<Full<Bytes>> {
        let (content_type, body, is_html) = match self.body {
            Body::Json(body) => ("application/json", body.to_string(), false),
            Body::Html(page) => ("text/html; charset=utf-8", page, true),
        };
        let mut response = Response::new(Full::new(Bytes::from(body)));
        *response.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        if is_html {
            for (name, value) in crate::ui::SECURITY_HEADERS {
                headers.insert(
                    hyper::header::HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                );
            }
        }
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
            let identity = match authenticate(app, &request).await {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            let gzip =
                header(&request, CONTENT_ENCODING).is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
            let body = match read_body(request, app.config.max_body_bytes).await {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            ingest::handle(app, &identity, Upload { body, gzip }).await
        }
        (&Method::GET, path) if query::parse_route(path).is_some() => {
            let identity = match authenticate(app, &request).await {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            let route = query::parse_route(path).expect("checked above");
            query::handle(app, &identity, route, request.uri().query()).await
        }
        (method, path) if crate::ui::is_ui_path(path) => {
            let assertion = request
                .headers()
                .get(crate::iap::IAP_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            crate::ui::handle(app, method, path, request.uri().query(), assertion).await
        }
        (_, "/healthz" | "/readyz" | "/v1/test-results/runs") => {
            Reply::error(405, "method not allowed")
        }
        (_, path) if query::parse_route(path).is_some() => Reply::error(405, "method not allowed"),
        _ => Reply::error(404, "not found"),
    }
}

/// Who is calling, from the bearer token; the error is the reply to send.
async fn authenticate(app: &App, request: &Request<Incoming>) -> Result<Identity, Reply> {
    let authorization = header(request, AUTHORIZATION);
    match app.verifier.verify(authorization.as_deref()).await {
        Ok(identity) => Ok(identity),
        Err(error @ AuthError::Forbidden(_)) => Err(Reply::error(403, error.to_string())),
        Err(error) => Err(Reply::error(401, error.to_string())),
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
