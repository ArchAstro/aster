//! The HTTP server and its routes.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::header::{
    HeaderName, HeaderValue, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE,
    TRANSFER_ENCODING,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::audit::{self, RequestCtx};
use crate::auth::{Identity, Principal};
use crate::limits;
use crate::test_results::ingest::{self, Upload};
use crate::test_results::{durations, query};
use crate::App;

/// A response before it is sent.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Body,
    /// Headers beyond the ones every response carries, such as `Retry-After`.
    pub headers: Vec<(&'static str, String)>,
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
    /// Plain text with the given content type, such as `/metrics`.
    Text(&'static str, String),
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: Body::Json(body),
            headers: Vec::new(),
        }
    }

    pub fn html(status: u16, page: String) -> Self {
        Self {
            status,
            body: Body::Html(page),
            headers: Vec::new(),
        }
    }

    pub fn bytes(status: u16, body: Bytes) -> Self {
        Self {
            status,
            body: Body::Bytes(body),
            headers: Vec::new(),
        }
    }

    pub fn redirect(location: String) -> Self {
        Self {
            status: 302,
            body: Body::Redirect(location),
            headers: Vec::new(),
        }
    }

    pub fn text(status: u16, content_type: &'static str, text: String) -> Self {
        Self {
            status,
            body: Body::Text(content_type, text),
            headers: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }

    /// `429` with how many seconds to wait.
    fn too_many(seconds: u64) -> Self {
        Self::error(429, "too many requests; slow down")
            .with_header("retry-after", limits::retry_after(seconds))
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({"error": message.into()}))
    }

    /// `hsts` adds `Strict-Transport-Security`: set when the server's public
    /// URL is https, on every response, errors and refusals included.
    fn into_response(self, hsts: bool) -> Response<Full<Bytes>> {
        let mut location = None;
        let mut content_type_override = None;
        let (content_type, body, is_html) = match self.body {
            Body::Json(body) => ("application/json", Bytes::from(body.to_string()), false),
            Body::Html(page) => ("text/html; charset=utf-8", Bytes::from(page), true),
            Body::Bytes(body) => ("application/octet-stream", body, false),
            Body::Redirect(url) => {
                location = Some(url);
                ("text/plain; charset=utf-8", Bytes::new(), false)
            }
            Body::Text(content_type, text) => {
                content_type_override = Some(content_type);
                ("text/plain", Bytes::from(text), false)
            }
        };
        let content_type = content_type_override.unwrap_or(content_type);
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
        headers.insert(
            hyper::header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if hsts {
            headers.insert(
                hyper::header::STRICT_TRANSPORT_SECURITY,
                HeaderValue::from_static("max-age=31536000; includeSubDomains"),
            );
        }
        for (name, value) in self.headers {
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
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

/// Which routes a listener answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// Everything: the dashboard, `/cli-auth`, health checks, `/metrics` and
    /// `/v1/*`. Sits behind IAP at the load balancer.
    Main,
    /// `/v1/*` only (`ASTER_SRV_API_ADDR`): the routes that carry their own
    /// bearer tokens and have no IAP in front. Anything else is a 404
    /// whatever the load balancer's URL map says, so a routing mistake
    /// cannot expose the dashboard or sign-in pages without IAP.
    Api,
}

/// Serve `app`'s main surface on `listener` until `shutdown` resolves, then
/// give open requests up to 30 seconds to finish.
pub async fn serve(
    app: Arc<App>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    serve_surface(app, listener, Surface::Main, shutdown).await
}

/// Serve only `/v1/*` on `listener` (see [`Surface::Api`]).
pub async fn serve_api(
    app: Arc<App>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    serve_surface(app, listener, Surface::Api, shutdown).await
}

async fn serve_surface(
    app: Arc<App>,
    listener: TcpListener,
    surface: Surface,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let graceful = GracefulShutdown::new();
    // One permit per open connection: past the cap, new ones wait in the
    // kernel's accept queue instead of costing a task and a socket each.
    let connections = Arc::new(Semaphore::new(app.config.max_connections));
    let hsts = app
        .config
        .public_url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://"));
    tokio::pin!(shutdown);
    loop {
        let permit = tokio::select! {
            permit = Arc::clone(&connections).acquire_owned() => permit.expect("never closed"),
            _ = &mut shutdown => break,
        };
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        crate::log::warn("accept failed", json!({"error": error.to_string()}));
                        continue;
                    }
                };
                let header_timeout = app.config.header_timeout;
                let app = Arc::clone(&app);
                let service = service_fn(move |request| {
                    let app = Arc::clone(&app);
                    async move {
                        let reply = handle(&app, surface, peer.ip(), request).await;
                        Ok::<_, Infallible>(reply.into_response(hsts))
                    }
                });
                let connection = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(header_timeout)
                    .max_buf_size(32 * 1024)
                    .serve_connection(TokioIo::new(stream), service);
                let connection = graceful.watch(connection);
                tokio::spawn(async move {
                    let _ = connection.await;
                    drop(permit);
                });
            }
            _ = &mut shutdown => break,
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(30), graceful.shutdown()).await;
    Ok(())
}

/// One request, from the connection's peer to the reply: the audit context,
/// the handler timeout, and the refusals that need no handler.
async fn handle(
    app: &App,
    surface: Surface,
    peer: std::net::IpAddr,
    request: Request<Incoming>,
) -> Reply {
    let forwarded = request
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    let client = limits::client_ip(
        Some(forwarded.as_str()).filter(|f| !f.is_empty()),
        peer,
        app.limits.trusted_proxy_hops,
    );
    let path = request.uri().path().to_string();
    let ctx = RequestCtx::new(
        audit::route_label(request.method(), &path),
        client.to_string(),
        Arc::clone(&app.metrics),
    );
    audit::scope(ctx, async {
        let work = guarded(app, surface, client, request);
        let reply = tokio::time::timeout(app.config.handler_timeout, work)
            .await
            .unwrap_or_else(|_| Reply::error(504, "the request took too long"));
        // Every 401 and 403 is audited, whichever handler produced it.
        if matches!(reply.status, 401 | 403) && !audit::was_audited() {
            let (reason, detail) = if reply.status == 401 {
                ("unauthorized", "the handler refused the credentials")
            } else {
                ("forbidden", "the handler refused the request")
            };
            audit::auth_failure(None, reason, detail);
        }
        reply
    })
    .await
}

/// What the request body may weigh on this route; 0 means it takes none.
fn body_limit(app: &App, method: &Method, path: &str) -> usize {
    match (method.as_str(), path) {
        ("POST", "/cli-auth" | "/v1/auth/token") => crate::cli_auth::MAX_FORM_BYTES,
        ("POST", "/v1/test-results/runs") => app.config.max_body_bytes,
        ("POST" | "PUT", path) if path.starts_with("/v1/cache/") => app.config.max_body_bytes,
        // The in-memory blob store's rewritten signed URL.
        ("PUT", path) if path.split('/').nth(2) == Some("cas") => app.config.max_body_bytes,
        _ => 0,
    }
}

async fn guarded(
    app: &App,
    surface: Surface,
    client: std::net::IpAddr,
    request: Request<Incoming>,
) -> Reply {
    let path = request.uri().path().to_string();
    let is_get = request.method() == Method::GET;
    if surface == Surface::Api {
        // Only /v1/*. Checked before anything else, including readiness.
        if !path.starts_with("/v1/") {
            return Reply::error(404, "not found");
        }
        // A bare liveness answer for the load balancer's health check: it
        // reveals only that the process is up, and is not authenticated
        // because the check carries no credentials.
        if is_get && path == "/v1/healthz" {
            return Reply::text(200, "text/plain; charset=utf-8", "ok".into());
        }
    }
    if surface == Surface::Main && is_get && path == "/metrics" {
        // Scrapers reach the pod directly. A request through the load
        // balancer always carries `X-Forwarded-For` and IAP's assertion, so
        // either header means it came from outside: 404.
        let headers = request.headers();
        if headers.contains_key("x-forwarded-for") || headers.contains_key(crate::iap::IAP_HEADER) {
            return Reply::error(404, "not found");
        }
        return Reply::text(
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            app.metrics.render(),
        );
    }
    if path.starts_with("/v1/") {
        let key = limits::ip_key(client);
        let mut verdict = app.limits.api_ip.check(&key);
        if verdict.is_ok() && request.method() == Method::POST && path == "/v1/auth/token" {
            verdict = app.limits.token_ip.check(&key);
        }
        if let Err(seconds) = verdict {
            audit::rate_limited("ip", None);
            return Reply::too_many(seconds);
        }
    }
    // Refuse an oversized or unexpected body before reading any of it.
    let limit = body_limit(app, request.method(), &path);
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    let chunked = request.headers().contains_key(TRANSFER_ENCODING);
    if declared.is_some_and(|length| length > limit) || (limit == 0 && chunked) {
        return Reply::error(
            413,
            if limit == 0 {
                "this route takes no body".to_string()
            } else {
                format!("the body is larger than {limit} bytes")
            },
        );
    }
    route(app, request).await
}

/// Whether `method path` is a `/v1` route that authenticates a bearer token.
fn is_v1_authed(method: &Method, path: &str) -> bool {
    match (method.as_str(), path) {
        ("POST", "/v1/test-results/runs") | ("GET", "/v1/test-results/durations") => true,
        ("GET", "/v1/cache/summary" | "/v1/cache/entries") => true,
        ("POST", path) if path.starts_with("/v1/cache/") => true,
        ("GET", path) => query::parse_route(path).is_some(),
        _ => false,
    }
}

/// Authenticate once, up front, on every `/v1` route that takes a token: a
/// bad token is audited and counted here whichever handler it was for, and a
/// good one is charged to its subject's rate limit. Handlers verify again
/// for their own policy; the check is a signature, not a query.
async fn preauth(app: &App, request: &Request<Incoming>) -> Result<(), Reply> {
    let authorization = header(request, AUTHORIZATION);
    let principal = match app.principal(authorization.as_deref()).await {
        Ok(principal) => principal,
        Err(error) => {
            let subject = match &error {
                crate::auth::AuthError::Forbidden(repository) => Some(repository.as_str()),
                _ => None,
            };
            audit::auth_failure(subject, error.reason_code(), &error.to_string());
            return Err(error.into_reply());
        }
    };
    let subject = match &principal {
        Principal::Ci(identity) => format!("ci:{}", identity.repository),
        Principal::Person(person) => format!("person:{}", person.subject),
    };
    app.limits.api_subject.check(&subject).map_err(|seconds| {
        audit::rate_limited("subject", Some(&subject));
        Reply::too_many(seconds)
    })
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
    if is_v1_authed(request.method(), request.uri().path()) {
        if let Err(reply) = preauth(app, &request).await {
            return reply;
        }
    }
    let path = request.uri().path().to_string();
    match (request.method(), path.as_str()) {
        (&Method::GET, "/healthz") => Reply::json(200, json!({"ok": true})),
        (&Method::GET, "/readyz") => match ready(app).await {
            Ok(()) => Reply::json(200, json!({"ok": true})),
            Err(error) => {
                // The error can carry connection and schema details; this
                // route is unauthenticated, so they go to the log only.
                crate::log::error(
                    "readiness check failed",
                    json!({"error": format!("{error:#}")}),
                );
                Reply::error(503, "the database is unavailable")
            }
        },
        (&Method::GET, "/cli-auth") => {
            let assertion = header_name(&request, crate::iap::IAP_HEADER);
            crate::cli_auth::confirm(app, assertion.as_deref(), request.uri().query()).await
        }
        (&Method::POST, "/cli-auth") => {
            let assertion = header_name(&request, crate::iap::IAP_HEADER);
            let body = match read_body(
                request,
                crate::cli_auth::MAX_FORM_BYTES,
                app.config.body_timeout,
            )
            .await
            {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            crate::cli_auth::authorize(app, assertion.as_deref(), &body).await
        }
        (&Method::POST, "/v1/auth/token") => {
            let body = match read_body(
                request,
                crate::cli_auth::MAX_FORM_BYTES,
                app.config.body_timeout,
            )
            .await
            {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            // Few exchanges run at once; the rest are told to retry rather
            // than queue for the auth pool.
            let Ok(_running) = Arc::clone(&app.limits.exchanges).try_acquire_owned() else {
                audit::rate_limited("concurrency", None);
                return Reply::error(503, "busy; retry shortly")
                    .with_header("retry-after", "1".into());
            };
            crate::cli_auth::exchange(app, &body).await
        }
        (&Method::POST, "/v1/test-results/runs") => {
            let identity = match authenticate(app, &request).await {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            // Uploads, like cache writes, may be limited to named workflows.
            if !crate::cache::workflow_allowed(&app.config, &identity) {
                return Reply::error(403, "this workflow may not upload test results");
            }
            let gzip =
                header(&request, CONTENT_ENCODING).is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
            let body = match read_body(request, app.config.max_body_bytes, app.config.body_timeout)
                .await
            {
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
        Ok(Principal::Person(person)) => {
            audit::auth_failure(
                Some(&person.subject),
                "person_token_not_allowed",
                "a person's token may only read the cache",
            );
            Err(Reply::error(
                403,
                "a person's token may only read the cache",
            ))
        }
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
        match read_body(request, app.config.max_body_bytes, app.config.body_timeout).await {
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

/// The body, at most `limit` bytes, all of it within `timeout`: a client
/// that trickles bytes holds the request for `timeout` and no longer.
async fn read_body(
    request: Request<Incoming>,
    limit: usize,
    timeout: Duration,
) -> Result<Bytes, Reply> {
    let read = Limited::new(request.into_body(), limit).collect();
    match tokio::time::timeout(timeout, read).await {
        Err(_) => Err(Reply::error(408, "the body took too long to arrive")),
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(error)) if error.downcast_ref::<LengthLimitError>().is_some() => Err(Reply::error(
            413,
            format!("the body is larger than {limit} bytes"),
        )),
        Ok(Err(_)) => Err(Reply::error(400, "cannot read the body")),
    }
}

async fn ready(app: &App) -> anyhow::Result<()> {
    let client = app.pool.get().await?;
    client.simple_query("SELECT 1").await?;
    Ok(())
}
