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

use crate::auth::{Identity, Principal};
use crate::test_results::ingest::{self, Upload};
use crate::test_results::{durations, query};
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
    /// Raw bytes, as a signed blob GET returns them.
    Bytes(Bytes),
    /// A `302` to this URL, with no body.
    Redirect(String),
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

    pub fn bytes(status: u16, body: Bytes) -> Self {
        Self {
            status,
            body: Body::Bytes(body),
        }
    }

    pub fn redirect(location: String) -> Self {
        Self {
            status: 302,
            body: Body::Redirect(location),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({"error": message.into()}))
    }

    fn into_response(self) -> Response<Full<Bytes>> {
        let mut location = None;
        let (content_type, body, is_html) = match self.body {
            Body::Json(body) => ("application/json", Bytes::from(body.to_string()), false),
            Body::Html(page) => ("text/html; charset=utf-8", Bytes::from(page), true),
            Body::Bytes(body) => ("application/octet-stream", body, false),
            Body::Redirect(url) => {
                location = Some(url);
                ("text/plain; charset=utf-8", Bytes::new(), false)
            }
        };
        let mut response = Response::new(Full::new(body));
        *response.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        // Sign-in pages, codes and tokens must not be stored by a cache.
        headers.insert(
            hyper::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
        if let Some(url) = location.and_then(|url| HeaderValue::from_str(&url).ok()) {
            headers.insert(hyper::header::LOCATION, url);
        }
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
    // Until the migrations are applied the tables may not match what the
    // handlers query. `/healthz` stays up so the process is not killed while
    // it waits for locks; `/readyz` reports the wait.
    if !app.schema_ready() && request.uri().path() != "/healthz" {
        let mut body = json!({"error": "the database schema is being migrated"});
        if request.uri().path() == "/readyz" {
            if let Some(failure) = app.migration_failure() {
                body["migration"] = json!(failure);
            }
        }
        return Reply::json(503, body);
    }
    let path = request.uri().path().to_string();
    match (request.method(), path.as_str()) {
        (&Method::GET, "/healthz") => Reply::json(200, json!({"ok": true})),
        (&Method::GET, "/readyz") => match ready(app).await {
            Ok(()) => Reply::json(200, json!({"ok": true})),
            Err(error) => Reply::error(503, format!("the database is unavailable: {error}")),
        },
        (&Method::GET, "/cli-auth") => {
            let assertion = header_name(&request, crate::iap::IAP_HEADER);
            crate::cli_auth::confirm(app, assertion.as_deref(), request.uri().query()).await
        }
        (&Method::POST, "/cli-auth") => {
            let assertion = header_name(&request, crate::iap::IAP_HEADER);
            let body = match read_body(request, crate::cli_auth::MAX_FORM_BYTES).await {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            crate::cli_auth::authorize(app, assertion.as_deref(), &body).await
        }
        (&Method::POST, "/v1/auth/token") => {
            let body = match read_body(request, crate::cli_auth::MAX_FORM_BYTES).await {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            crate::cli_auth::exchange(app, &body).await
        }
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
        (&Method::GET, "/v1/test-results/durations") => {
            let identity = match authenticate(app, &request).await {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            durations::handle(app, &identity, request.uri().query()).await
        }
        (&Method::GET, path) if query::parse_route(path).is_some() => {
            let identity = match authenticate(app, &request).await {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            let route = query::parse_route(path).expect("checked above");
            query::handle(app, &identity, route, request.uri().query()).await
        }
        (&Method::POST, path) if path.starts_with("/v1/cache/") => {
            cache_request(app, request, "POST", path).await
        }
        (&Method::GET, "/v1/cache/summary" | "/v1/cache/entries") => {
            cache_request(app, request, "GET", &path).await
        }
        // A rewritten signed blob URL: `PUT|GET /<bucket>/cas/<sha256>`.
        // Only the in-memory store answers it. A GCS-backed server, which is
        // every production deploy, falls through to 404.
        (method, path)
            if app.cache_store.is_memory()
                && matches!(method, &Method::PUT | &Method::GET)
                && path.split('/').nth(2) == Some("cas") =>
        {
            let method = method.as_str().to_string();
            cache_request(app, request, &method, path).await
        }
        (method, path) if crate::ui::is_ui_path(path) => {
            let assertion = request
                .headers()
                .get(crate::iap::IAP_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            crate::ui::handle(app, method, path, request.uri().query(), assertion).await
        }
        (
            _,
            "/healthz"
            | "/readyz"
            | "/cli-auth"
            | "/v1/auth/token"
            | "/v1/test-results/runs"
            | "/v1/test-results/durations",
        ) => Reply::error(405, "method not allowed"),
        (_, path) if query::parse_route(path).is_some() => Reply::error(405, "method not allowed"),
        _ => Reply::error(404, "not found"),
    }
}

/// Which CI job is calling, from the bearer token; the error is the reply
/// to send. A person's token is valid but is for reading the cache only.
async fn authenticate(app: &App, request: &Request<Incoming>) -> Result<Identity, Reply> {
    let authorization = header(request, AUTHORIZATION);
    match app.principal(authorization.as_deref()).await {
        Ok(Principal::Ci(identity)) => Ok(identity),
        Ok(Principal::Person(_)) => Err(Reply::error(
            403,
            "a person's token may only read the cache",
        )),
        Err(error) => Err(error.into_reply()),
    }
}

fn header_name(request: &Request<Incoming>, name: &str) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn header(request: &Request<Incoming>, name: hyper::header::HeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

async fn cache_request(app: &App, request: Request<Incoming>, method: &str, path: &str) -> Reply {
    let authorization = header(&request, AUTHORIZATION);
    let query = request.uri().query().map(str::to_string);
    // JSON cache routes carry a small body. The rewritten blob GET does not,
    // and waiting for one stalls the client until its request timeout.
    let body = if method == "GET" {
        Bytes::new()
    } else {
        match read_body(request, app.config.max_body_bytes).await {
            Ok(body) => body,
            Err(reply) => return reply,
        }
    };
    crate::cache::handle(
        app,
        method,
        path,
        &crate::cache::Headers {
            authorization: authorization.as_deref(),
            query: query.as_deref(),
        },
        body,
    )
    .await
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
