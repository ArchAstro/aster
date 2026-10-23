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
use crate::routes::{self, Handler};
use crate::test_results::ingest::{self, Upload};
use crate::test_results::{durations, query};
use crate::App;

/// The most a cache lookup or upload request may send after it is
/// authenticated: a list of up to 1,024 digests is about 100 KiB.
const SMALL_BODY_BYTES: usize = 1 << 20;

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
    pub(crate) fn too_many(seconds: u64) -> Self {
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
    let per_peer = limits::Concurrency::new(app.config.max_connections_per_ip);
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
                // With no trusted proxy the peer is the client, so its open
                // connections can be capped here. Behind a load balancer the
                // peer is the balancer; the per-client cap is on in-flight
                // requests in `handle`, by the forwarded address.
                let connection_slot = if app.config.trusted_proxy_hops == 0 {
                    match per_peer.try_acquire(&limits::ip_key(peer.ip().to_canonical())) {
                        Some(slot) => Some(slot),
                        None => {
                            app.metrics.rate_limited("other", "connections_per_ip");
                            continue;
                        }
                    }
                } else {
                    None
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
                    drop(connection_slot);
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
    let forwarded = limits::forwarded_for(
        request
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    let client = limits::client_ip(forwarded.as_deref(), peer, app.limits.trusted_proxy_hops);
    let path = request.uri().path().to_string();
    let ctx = RequestCtx::new(
        routes::label(request.method().as_str(), &path),
        client.to_string(),
        Arc::clone(&app.metrics),
        Arc::clone(&app.audit_sampler),
    );
    audit::scope(ctx, async {
        // A client IP holds only so many requests open at once, so slow
        // bodies from one address cannot take every connection slot.
        let Some(_slot) = app
            .limits
            .requests_per_ip
            .try_acquire(&limits::ip_key(client))
        else {
            audit::rate_limited("ip_concurrency", None);
            return Reply::too_many(1);
        };
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
/// Each handler names its own cap, so a route added to the table with a
/// handler that reads a body must state one here.
fn body_limit(app: &App, route: Option<&routes::Route>, path: &str) -> usize {
    let Some(route) = route else { return 0 };
    match route.handler {
        Handler::CliAuthAuthorize | Handler::TokenExchange => crate::cli_auth::MAX_FORM_BYTES,
        Handler::ResultsUpload => app.config.max_body_bytes,
        // A lookup or upload request is a list of keys or digests; only a
        // commit carries a manifest.
        Handler::CacheLookup | Handler::CacheWrite if path != "/v1/cache/commit" => {
            SMALL_BODY_BYTES.min(app.config.max_body_bytes)
        }
        Handler::CacheWrite => app.config.max_body_bytes,
        // The in-memory blob store's rewritten signed URL takes an upload.
        Handler::SignedBlob if route.method == "PUT" => app.config.max_body_bytes,
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
        // The router and the auth gate are the main surface's, unchanged: a
        // `/v1/` route is gated the same on both listeners. A route that
        // relies on IAP, which this listener has none of, is not served.
        if !path.starts_with("/v1/")
            || routes::lookup(request.method().as_str(), &path)
                .is_some_and(|route| route.auth == routes::Auth::Iap)
        {
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
    let matched = routes::lookup(request.method().as_str(), &path);
    let limit = body_limit(app, matched, &path);
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    let chunked = request.headers().contains_key(TRANSFER_ENCODING);
    // A known path with the wrong method is a 405 whatever it declares: no
    // handler runs and no body is read.
    let wrong_method = matched.is_none() && routes::path_known(&path);
    if !wrong_method && (declared.is_some_and(|length| length > limit) || (limit == 0 && chunked)) {
        return Reply::error(
            413,
            if limit == 0 {
                "this route takes no body".to_string()
            } else {
                format!("the body is larger than {limit} bytes")
            },
        );
    }
    route(app, client, request).await
}

async fn route(app: &App, client: std::net::IpAddr, request: Request<Incoming>) -> Reply {
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
    let method = request.method().as_str().to_string();
    let route = routes::lookup(&method, &path);
    // Authenticate before anything else runs or any body is read.
    let principal = if routes::needs_gate(route, &path) {
        let authorization = header(&request, AUTHORIZATION);
        match routes::gate(app, route, authorization.as_deref()).await {
            Ok(principal) => principal,
            Err(reply) => return reply,
        }
    } else {
        None
    };
    let Some(route) = route else {
        return if routes::path_known(&path) {
            Reply::error(405, "method not allowed")
        } else {
            Reply::error(404, "not found")
        };
    };
    match route.handler {
        Handler::Healthz => Reply::json(200, json!({"ok": true})),
        Handler::Readyz => match ready(app).await {
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
        Handler::CliAuthConfirm => {
            let browser = cli_browser(&request, client);
            crate::cli_auth::confirm(app, &browser, request.uri().query()).await
        }
        Handler::CliAuthAuthorize => {
            let browser = cli_browser(&request, client);
            let body = match read_body(
                request,
                crate::cli_auth::MAX_FORM_BYTES,
                app.config.unauth_body_timeout,
            )
            .await
            {
                Ok(body) => body,
                Err(reply) => return reply,
            };
            crate::cli_auth::authorize(app, &browser, &body).await
        }
        // The gate has already checked the bearer: a signed-in person, never
        // a CI token. The route takes no body.
        Handler::TokenRevoke => match principal {
            Some(Principal::Person(person)) => crate::cli_auth::revoke(app, &person).await,
            _ => Reply::error(403, "only a signed-in person can sign out"),
        },
        Handler::TokenExchange => {
            let body = match read_body(
                request,
                crate::cli_auth::MAX_FORM_BYTES,
                app.config.unauth_body_timeout,
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
        Handler::ResultsUpload => {
            let Some(Principal::Ci(identity)) = principal else {
                return Reply::error(403, "a person's token is read-only");
            };
            // The gate has applied the results writer rule (named workflows,
            // no `pull_request_target`) before this point.
            let Some(_large) = large_body_permit(app) else {
                return busy();
            };
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
        Handler::Durations => {
            let identity = match read_identity(app, principal, &request) {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            durations::handle(app, &identity, request.uri().query()).await
        }
        Handler::ResultsRead => {
            let identity = match read_identity(app, principal, &request) {
                Ok(identity) => identity,
                Err(reply) => return reply,
            };
            let Some(route) = query::parse_route(&path) else {
                return Reply::error(404, "not found");
            };
            query::handle(app, &identity, route, request.uri().query()).await
        }
        Handler::CacheRead | Handler::CacheLookup | Handler::CacheWrite => {
            // A commit carries a manifest of up to the body cap; only a few
            // run at once. The permit is held until the reply is built.
            let _large = if route.handler == Handler::CacheWrite && path == "/v1/cache/commit" {
                match large_body_permit(app) {
                    Some(permit) => Some(permit),
                    None => return busy(),
                }
            } else {
                None
            };
            cache_request(
                app,
                request,
                &method,
                &path,
                body_limit(app, Some(route), &path),
            )
            .await
        }
        // Only the in-memory store answers a rewritten signed blob URL. A
        // GCS-backed server, which is every production deploy, has none.
        Handler::SignedBlob => {
            if app.config.dev_blob_route
                && app.cache_store.is_memory()
                && path.split('/').nth(2) == Some("cas")
            {
                cache_request(
                    app,
                    request,
                    &method,
                    &path,
                    body_limit(app, Some(route), &path),
                )
                .await
            } else {
                Reply::error(404, "not found")
            }
        }
        Handler::Ui => {
            let assertion = header_name(&request, crate::iap::IAP_HEADER);
            let http_method = request.method().clone();
            crate::ui::handle(app, &http_method, &path, request.uri().query(), assertion).await
        }
    }
}

/// A permit to read one large body (a commit's manifest or a test-results
/// upload), or `None` when [`crate::Config::max_large_bodies`] are already
/// running. Held for the rest of the request.
fn large_body_permit(app: &App) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let permit = Arc::clone(&app.limits.large_bodies)
        .try_acquire_owned()
        .ok();
    if permit.is_none() {
        audit::rate_limited("large_body", None);
    }
    permit
}

fn busy() -> Reply {
    Reply::error(503, "busy; retry shortly").with_header("retry-after", "1".into())
}

/// Who is reading, for the GET routes over test results. A CI token reads
/// its own repository. A signed-in person reads any repository on the
/// allowlist, named by `?repo=owner/name`, so the identity returned for them
/// is one for that repository and the handlers apply their usual
/// own-repository rule to it.
fn read_identity(
    app: &App,
    principal: Option<Principal>,
    request: &Request<Incoming>,
) -> Result<Identity, Reply> {
    let principal = principal.ok_or_else(|| Reply::error(401, "send a bearer token"))?;
    if let Principal::Person(person) = &principal {
        let pairs = query::parse_query(request.uri().query())
            .ok_or_else(|| Reply::error(400, "the query is not valid"))?;
        let repo = pairs
            .iter()
            .find(|(key, _)| key == "repo")
            .map(|(_, value)| value.as_str());
        if let Some(repo) = repo {
            crate::cache::person_may_browse(app, person, repo)
                .map_err(|message| Reply::error(403, message))?;
        }
        return routes::identity_for(principal, repo);
    }
    routes::identity_for(principal, None)
}

/// What `/cli-auth` reads from a browser request.
fn cli_browser(request: &Request<Incoming>, client: std::net::IpAddr) -> crate::cli_auth::Browser {
    crate::cli_auth::Browser {
        assertion: header_name(request, crate::iap::IAP_HEADER),
        origin: header_name(request, "origin"),
        referer: header_name(request, "referer"),
        client_ip: client,
        user_agent: header_name(request, "user-agent"),
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

async fn cache_request(
    app: &App,
    request: Request<Incoming>,
    method: &str,
    path: &str,
    limit: usize,
) -> Reply {
    let authorization = header(&request, AUTHORIZATION);
    let query = request.uri().query().map(str::to_string);
    // JSON cache routes carry a small body. The rewritten blob GET does not,
    // and waiting for one stalls the client until its request timeout.
    let body = if method == "GET" {
        Bytes::new()
    } else {
        match read_body(request, limit, app.config.body_timeout).await {
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
