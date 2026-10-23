//! Audit log lines and the request context they are filled from.
//!
//! Every line is one JSON object on stdout like any log line, with
//! `"audit": true` and an `event`:
//!
//! | event | when | fields |
//! |-------|------|--------|
//! | `token_mint` | `/cli-auth` issued a one-time code | `subject`, `email`, `port` |
//! | `token_exchange` | `/v1/auth/token` finished | `outcome` (`success`/`failure`), `subject` if known, `reason` on failure |
//! | `token_revoke` | a token was revoked ([`token_revoke`], called by the revoke endpoint) | `subject`, `by` |
//! | `auth_failure` | any 401 or 403 | `subject` if known, `reason`, `detail` |
//! | `rate_limited` | a limit refused the request | `scope`, `subject` if known |
//!
//! Each carries `route` (a fixed template, see [`route_label`]) and
//! `client_ip` from the request. Tokens, codes, verifiers and assertions are
//! never fields: callers pass reasons and subjects only, and [`crate::log`]
//! redacts token-shaped text as a second line of defence.
//!
//! The route and client IP reach these functions through a task-local set by
//! the server for each request, so handlers call `audit::auth_failure(..)`
//! without threading request details through their signatures.
//!
//! Alert on spikes in Cloud Monitoring with a log-based metric over
//! `jsonPayload.audit=true AND jsonPayload.event="auth_failure"`, or with
//! Prometheus: `sum(rate(aster_srv_auth_failures_total[5m])) > 1`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::metrics::Metrics;

/// What is known about the request being served.
pub struct RequestCtx {
    pub route: &'static str,
    pub client_ip: String,
    pub metrics: Arc<Metrics>,
    /// Set once a failure was audited, so the server's catch-all does not
    /// log the same 401 or 403 twice.
    audited: AtomicBool,
}

impl RequestCtx {
    pub fn new(route: &'static str, client_ip: String, metrics: Arc<Metrics>) -> Self {
        Self {
            route,
            client_ip,
            metrics,
            audited: AtomicBool::new(false),
        }
    }
}

tokio::task_local! {
    static CTX: RequestCtx;
}

/// Run `future` with `ctx` as the current request.
pub async fn scope<F: Future>(ctx: RequestCtx, future: F) -> F::Output {
    CTX.scope(ctx, future).await
}

/// Whether the current request already logged an auth failure.
pub fn was_audited() -> bool {
    CTX.try_with(|ctx| ctx.audited.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// The templated route of a request, a bounded set safe as a metric label.
pub fn route_label(method: &hyper::Method, path: &str) -> &'static str {
    use crate::test_results::query;
    match path {
        "/healthz" => "/healthz",
        "/readyz" => "/readyz",
        "/metrics" => "/metrics",
        "/cli-auth" => "/cli-auth",
        "/v1/healthz" => "/v1/healthz",
        "/v1/auth/token" => "/v1/auth/token",
        "/v1/test-results/runs" => "/v1/test-results/runs",
        "/v1/test-results/durations" => "/v1/test-results/durations",
        p if p == "/ui" || p.starts_with("/ui/") => "/ui",
        p if p.starts_with("/v1/test-results/") && query::parse_route(p).is_some() => {
            "/v1/test-results/query"
        }
        p if p.starts_with("/v1/cache/") => "/v1/cache",
        p if p.starts_with("/v1/") => "/v1/other",
        _ => {
            let _ = method;
            "other"
        }
    }
}

fn emit(event: &str, route: Option<&'static str>, fields: Value) {
    let (ip, route) = CTX
        .try_with(|ctx| (Some(ctx.client_ip.clone()), Some(ctx.route)))
        .unwrap_or((None, route));
    let mut line = Map::new();
    line.insert("audit".into(), json!(true));
    line.insert("event".into(), json!(event));
    if let Some(route) = route {
        line.insert("route".into(), json!(route));
    }
    if let Some(ip) = ip {
        line.insert("client_ip".into(), json!(ip));
    }
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    crate::log::info("audit", Value::Object(line));
}

fn metrics(f: impl FnOnce(&Metrics, &'static str)) {
    let _ = CTX.try_with(|ctx| f(&ctx.metrics, ctx.route));
}

fn subject_field(subject: Option<&str>) -> Value {
    subject.map_or(Value::Null, |subject| json!(subject))
}

/// A request was refused for who it is (401) or what it may do (403).
/// `reason` is a fixed code; `detail` is human text that never holds a token.
pub fn auth_failure(subject: Option<&str>, reason: &'static str, detail: &str) {
    let _ = CTX.try_with(|ctx| ctx.audited.store(true, Ordering::Relaxed));
    metrics(|m, route| m.auth_failure(route, reason));
    let detail: String = detail.chars().take(200).collect();
    emit(
        "auth_failure",
        None,
        json!({"subject": subject_field(subject), "reason": reason, "detail": detail}),
    );
}

/// A limit refused the request.
pub fn rate_limited(scope: &'static str, subject: Option<&str>) {
    metrics(|m, route| m.rate_limited(route, scope));
    emit(
        "rate_limited",
        None,
        json!({"scope": scope, "subject": subject_field(subject)}),
    );
}

/// `/cli-auth` issued a one-time code to `subject`.
pub fn token_mint(subject: &str, email: &str, port: u16) {
    emit(
        "token_mint",
        None,
        json!({"subject": subject, "email": email, "port": port}),
    );
}

/// `/v1/auth/token` traded a code for a token.
pub fn exchange_succeeded(subject: &str) {
    emit(
        "token_exchange",
        None,
        json!({"outcome": "success", "subject": subject}),
    );
}

/// `/v1/auth/token` refused. Counts as an auth failure.
pub fn exchange_failed(subject: Option<&str>, reason: &'static str) {
    let _ = CTX.try_with(|ctx| ctx.audited.store(true, Ordering::Relaxed));
    metrics(|m, route| m.auth_failure(route, reason));
    emit(
        "token_exchange",
        None,
        json!({"outcome": "failure", "subject": subject_field(subject), "reason": reason}),
    );
}

/// The revoke endpoint calls this once it has revoked `subject`'s tokens;
/// `by` is who asked (the subject itself, or an operator).
pub fn token_revoke(subject: &str, by: &str) {
    emit("token_revoke", None, json!({"subject": subject, "by": by}));
}
