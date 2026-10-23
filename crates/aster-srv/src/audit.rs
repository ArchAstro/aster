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
//! The three failure events (`auth_failure`, `rate_limited`, a failed
//! `token_exchange`) are sampled: the first `ASTER_SRV_LOG_SAMPLE_FIRST`
//! (default 5) lines per `(event, reason, route, client_ip)` in each
//! `ASTER_SRV_LOG_SAMPLE_WINDOW_SECS` window (default 60) are logged, and when
//! the window ends one `audit_summary` line per key says how many more
//! happened (`suppressed`). The counters in [`crate::metrics`] are bumped
//! before sampling and stay exact. Mints, successful exchanges and revokes
//! are never sampled.
//!
//! Each carries `route` (a fixed template, see [`crate::routes::label`]) and
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

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::metrics::Metrics;

/// What is known about the request being served.
pub struct RequestCtx {
    pub route: &'static str,
    pub client_ip: String,
    pub metrics: Arc<Metrics>,
    pub sampler: Arc<Sampler>,
    /// Set once a failure was audited, so the server's catch-all does not
    /// log the same 401 or 403 twice.
    audited: AtomicBool,
}

impl RequestCtx {
    pub fn new(
        route: &'static str,
        client_ip: String,
        metrics: Arc<Metrics>,
        sampler: Arc<Sampler>,
    ) -> Self {
        Self {
            route,
            client_ip,
            metrics,
            sampler,
            audited: AtomicBool::new(false),
        }
    }
}

/// Distinct `(event, reason, route, client_ip)` keys sampled at once. Past
/// it, a new key is counted under `client_ip = "*"`, so a flood of spoofed
/// addresses cannot grow the table.
const MAX_SAMPLE_KEYS: usize = 10_000;

type SampleKey = (&'static str, &'static str, &'static str, String);

struct Seen {
    logged: u32,
    suppressed: u64,
}

struct Window {
    started: Instant,
    seen: HashMap<SampleKey, Seen>,
}

/// Logs the first few failures per key in a window and a summary of the
/// rest. See the module docs.
pub struct Sampler {
    first: u32,
    window: Duration,
    state: Mutex<Window>,
}

impl Sampler {
    pub fn new(first: u32, window: Duration) -> Self {
        Self {
            first,
            window,
            state: Mutex::new(Window {
                started: Instant::now(),
                seen: HashMap::new(),
            }),
        }
    }

    /// Whether to log this occurrence. Also closes a window that has ended
    /// and logs its summaries.
    pub fn admit(
        &self,
        event: &'static str,
        reason: &'static str,
        route: &'static str,
        ip: &str,
    ) -> bool {
        self.admit_at(event, reason, route, ip, Instant::now())
    }

    pub fn admit_at(
        &self,
        event: &'static str,
        reason: &'static str,
        route: &'static str,
        ip: &str,
        now: Instant,
    ) -> bool {
        let (closed, admitted) = {
            let mut window = self.state.lock().unwrap();
            let closed = self.roll(&mut window, now);
            let ip = if window.seen.len() >= MAX_SAMPLE_KEYS
                && !window
                    .seen
                    .contains_key(&(event, reason, route, ip.to_string()))
            {
                "*"
            } else {
                ip
            };
            let seen = window
                .seen
                .entry((event, reason, route, ip.to_string()))
                .or_insert(Seen {
                    logged: 0,
                    suppressed: 0,
                });
            let admitted = seen.logged < self.first;
            if admitted {
                seen.logged += 1;
            } else {
                seen.suppressed += 1;
            }
            (closed, admitted)
        };
        self.log_summaries(closed);
        admitted
    }

    /// Close the window if it has ended and log what it suppressed. The
    /// server calls this on a timer, so a quiet period still gets its summary.
    pub fn flush(&self) {
        self.flush_at(Instant::now());
    }

    pub fn flush_at(&self, now: Instant) {
        let closed = self.roll(&mut self.state.lock().unwrap(), now);
        self.log_summaries(closed);
    }

    /// Log every pending summary now, whether or not the window ended. For
    /// shutdown.
    pub fn flush_all(&self) {
        let closed = {
            let mut window = self.state.lock().unwrap();
            window.started = Instant::now();
            std::mem::take(&mut window.seen)
        };
        self.log_summaries(closed);
    }

    /// Start a new window if this one has ended, returning its entries.
    fn roll(&self, window: &mut Window, now: Instant) -> HashMap<SampleKey, Seen> {
        if now.saturating_duration_since(window.started) < self.window {
            return HashMap::new();
        }
        window.started = now;
        std::mem::take(&mut window.seen)
    }

    fn log_summaries(&self, closed: HashMap<SampleKey, Seen>) {
        let mut lines: Vec<_> = closed
            .into_iter()
            .filter(|(_, seen)| seen.suppressed > 0)
            .collect();
        lines.sort_by(|a, b| a.0.cmp(&b.0));
        for ((event, reason, route, ip), seen) in lines {
            crate::log::info(
                "audit",
                json!({
                    "audit": true,
                    "event": "audit_summary",
                    "of": event,
                    "reason": reason,
                    "route": route,
                    "client_ip": ip,
                    "suppressed": seen.suppressed,
                    "logged": seen.logged,
                    "window_secs": self.window.as_secs(),
                }),
            );
        }
    }

    /// Flush on a timer for as long as `sampler` is alive.
    pub fn spawn_flush(sampler: &Arc<Self>) {
        let weak = Arc::downgrade(sampler);
        let every = sampler.window;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await;
            loop {
                tick.tick().await;
                match weak.upgrade() {
                    Some(sampler) => sampler.flush(),
                    None => break,
                }
            }
        });
    }
}

/// Whether the current request should log a failure of this kind: the
/// first few per `(event, reason, route, client_ip)` per window. Always true
/// outside a request.
pub fn admit(event: &'static str, reason: &'static str) -> bool {
    CTX.try_with(|ctx| ctx.sampler.admit(event, reason, ctx.route, &ctx.client_ip))
        .unwrap_or(true)
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
    if !admit("auth_failure", reason) {
        return;
    }
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
    if !admit("rate_limited", scope) {
        return;
    }
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
    if !admit("token_exchange", reason) {
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn summaries() -> Vec<Value> {
        crate::log::captured()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["event"] == "audit_summary")
            .collect()
    }

    #[test]
    fn the_first_few_per_key_are_admitted_and_the_rest_summarised_when_the_window_ends() {
        crate::log::start_capture();
        let sampler = Sampler::new(3, Duration::from_secs(60));
        let start = Instant::now();
        let admit = |ip: &str, at: u64| {
            sampler.admit_at(
                "auth_failure",
                "n15_unit_reason",
                "/v1/cache",
                ip,
                start + Duration::from_secs(at),
            )
        };
        let admitted = (0..10).filter(|n| admit("203.0.113.1", *n)).count();
        assert_eq!(admitted, 3);
        // Another client IP, and another reason, have their own budget.
        assert!(admit("203.0.113.2", 11));
        assert!(sampler.admit_at("auth_failure", "other", "/v1/cache", "203.0.113.1", start));
        // Nothing is summarised while the window is open.
        sampler.flush_at(start + Duration::from_secs(59));
        assert!(summaries().iter().all(|s| s["reason"] != "n15_unit_reason"));
        // The window ends: one summary for the key that suppressed lines.
        sampler.flush_at(start + Duration::from_secs(61));
        let ours: Vec<_> = summaries()
            .into_iter()
            .filter(|s| s["reason"] == "n15_unit_reason")
            .collect();
        assert_eq!(ours.len(), 1, "{ours:?}");
        assert_eq!(ours[0]["client_ip"], "203.0.113.1");
        assert_eq!(ours[0]["suppressed"], 7);
        assert_eq!(ours[0]["logged"], 3);
        assert_eq!(ours[0]["of"], "auth_failure");
        // A new window starts with a fresh budget.
        assert!(admit("203.0.113.1", 62));
    }

    #[test]
    fn a_flood_of_distinct_addresses_cannot_grow_the_table() {
        let sampler = Sampler::new(1, Duration::from_secs(60));
        let now = Instant::now();
        for n in 0..(MAX_SAMPLE_KEYS + 500) {
            sampler.admit_at("auth_failure", "r", "/v1/cache", &format!("ip-{n}"), now);
        }
        let window = sampler.state.lock().unwrap();
        assert!(window.seen.len() <= MAX_SAMPLE_KEYS + 1);
        // The overflow is counted under one aggregate key.
        assert!(window
            .seen
            .contains_key(&("auth_failure", "r", "/v1/cache", "*".to_string())));
    }
}
