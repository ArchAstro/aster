//! Webhook notifications: configured endpoints hear about tests changing
//! state.
//!
//! The classifier records each notification in `webhooks.deliveries` in the
//! same transaction as the change it reports (an outbox), one row per
//! subscribed endpoint, so a change is never announced without happening
//! nor lost after it happens. A separate loop sends due rows and retries
//! failures with exponential backoff; an endpoint that is down only delays
//! its own deliveries, never ingest or classification. Delivered and
//! abandoned rows are kept for the retention period as the delivery log.
//!
//! Each delivery is a `POST` of a JSON body with:
//!
//! - `X-Aster-Event`: the event type, e.g. `test.quarantined`
//! - `X-Aster-Delivery`: the delivery's id, the same on every retry
//! - `X-Aster-Signature`: `sha256=` and the hex HMAC-SHA256 of the body,
//!   keyed by the endpoint's secret

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Migration;
use crate::App;

pub const MIGRATIONS: &[Migration] = &[Migration {
    feature: "webhooks",
    version: 1,
    sql: r#"
CREATE SCHEMA IF NOT EXISTS webhooks;

CREATE TABLE webhooks.deliveries (
    id bigserial PRIMARY KEY,
    endpoint text NOT NULL,
    event text NOT NULL,
    body text NOT NULL,
    created_at timestamptz NOT NULL,
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL,
    delivered_at timestamptz,
    abandoned_at timestamptz,
    last_status integer,
    last_error text
);
CREATE INDEX deliveries_due ON webhooks.deliveries (next_attempt_at)
    WHERE delivered_at IS NULL AND abandoned_at IS NULL;
CREATE INDEX deliveries_created ON webhooks.deliveries (created_at);
"#,
}];

/// The event types an endpoint can subscribe to.
pub const EVENTS: [&str; 4] = [
    "test.quarantined",
    "test.released",
    "test.expired",
    "test.broken",
];

/// How often the sender looks for due deliveries.
pub const INTERVAL: Duration = Duration::from_secs(15);

/// Attempts before a delivery is abandoned. With the backoff below, the
/// last one is about 14.5 hours after the first.
const MAX_ATTEMPTS: i32 = 12;

/// How long one attempt may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long a claimed delivery is held before another sender may take it.
const LEASE: chrono::Duration = chrono::Duration::minutes(5);

/// Deliveries one pass claims.
const BATCH: i64 = 50;

/// One configured endpoint.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Names the endpoint in the delivery log; unique.
    pub name: String,
    pub url: String,
    pub secret: String,
    /// The event types it receives.
    pub events: Vec<String>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("secret", &"[redacted]")
            .field("events", &self.events)
            .finish()
    }
}

impl Endpoint {
    /// `ASTER_SRV_WEBHOOKS`: a JSON array of
    /// `{"name", "url", "secret", "events": [...]}`.
    pub fn parse_all(value: &str) -> Result<Vec<Self>> {
        let endpoints: Vec<Self> = serde_json::from_str(value)
            .context("ASTER_SRV_WEBHOOKS must be a JSON array of {name, url, secret, events}")?;
        for (index, endpoint) in endpoints.iter().enumerate() {
            let name = &endpoint.name;
            if name.trim().is_empty() {
                bail!("webhook {index} has no name");
            }
            if endpoints[..index].iter().any(|other| other.name == *name) {
                bail!("two webhooks are named {name}");
            }
            if !allowed_url(&endpoint.url) {
                bail!(
                    "webhook {name}'s url must be https:// (http:// only for localhost): {}",
                    endpoint.url
                );
            }
            if endpoint.secret.len() < 16 {
                bail!("webhook {name}'s secret must be at least 16 characters");
            }
            if endpoint.events.is_empty() {
                bail!("webhook {name} subscribes to no events");
            }
            if let Some(unknown) = endpoint
                .events
                .iter()
                .find(|event| !EVENTS.contains(&event.as_str()))
            {
                bail!(
                    "webhook {name} names an unknown event {unknown}; known: {}",
                    EVENTS.join(", ")
                );
            }
        }
        Ok(endpoints)
    }

    fn wants(&self, event: &str) -> bool {
        self.events.iter().any(|wanted| wanted == event)
    }
}

fn allowed_url(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// A notification to send to every endpoint subscribed to `event`.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub event: &'static str,
    pub body: Value,
}

/// Record `notifications` for the endpoints that want them, in the caller's
/// transaction. Returns the deliveries recorded.
pub async fn enqueue(
    transaction: &tokio_postgres::Transaction<'_>,
    endpoints: &[Endpoint],
    notifications: &[Notification],
    now: DateTime<Utc>,
) -> Result<u64> {
    let mut recorded = 0;
    for notification in notifications {
        let body = notification.body.to_string();
        for endpoint in endpoints.iter().filter(|e| e.wants(notification.event)) {
            recorded += transaction
                .execute(
                    "INSERT INTO webhooks.deliveries \
                     (endpoint, event, body, created_at, next_attempt_at) \
                     VALUES ($1, $2, $3, $4, $4)",
                    &[&endpoint.name, &notification.event, &body, &now],
                )
                .await?;
        }
    }
    Ok(recorded)
}

/// `sha256=` and the hex HMAC-SHA256 of `body` under `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    use aws_lc_rs::hmac;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// The wait before attempt `attempts + 1`: 30 seconds, doubling, at most
/// six hours.
fn backoff(attempts: i32) -> chrono::Duration {
    let doublings = u32::try_from(attempts.saturating_sub(1))
        .unwrap_or(0)
        .min(16);
    let seconds = (30i64 << doublings).min(6 * 60 * 60);
    chrono::Duration::seconds(seconds)
}

/// What a sending pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub delivered: u64,
    /// Failed attempts that will be tried again.
    pub retrying: u64,
    pub abandoned: u64,
}

/// Send the deliveries due at `now`.
pub async fn deliver_once(app: &App, now: DateTime<Utc>) -> Result<Report> {
    let client = app.pool.get().await?;
    let lease = now + LEASE;
    let claimed = client
        .query(
            "UPDATE webhooks.deliveries SET attempts = attempts + 1, next_attempt_at = $2 \
             WHERE id IN (SELECT id FROM webhooks.deliveries \
                          WHERE delivered_at IS NULL AND abandoned_at IS NULL \
                            AND next_attempt_at <= $1 \
                          ORDER BY next_attempt_at, id LIMIT $3 \
                          FOR UPDATE SKIP LOCKED) \
             RETURNING id, endpoint, event, body, attempts",
            &[&now, &lease, &BATCH],
        )
        .await?;
    let http = crate::http_client()
        .timeout(TIMEOUT)
        .build()
        .context("cannot build the webhook client")?;
    let mut sends = tokio::task::JoinSet::new();
    for row in claimed {
        let id: i64 = row.get("id");
        let name: String = row.get("endpoint");
        let event: String = row.get("event");
        let body: String = row.get("body");
        let attempts: i32 = row.get("attempts");
        let endpoint = app.config.webhooks.iter().find(|e| e.name == name).cloned();
        let http = http.clone();
        sends.spawn(async move {
            let outcome = match endpoint {
                None => Err((
                    None,
                    "the endpoint is no longer configured".to_string(),
                    true,
                )),
                Some(endpoint) => send(&http, &endpoint, id, &event, body)
                    .await
                    .map_err(|(status, error)| (status, error, false)),
            };
            (id, attempts, outcome)
        });
    }
    let mut report = Report::default();
    while let Some(joined) = sends.join_next().await {
        let (id, attempts, outcome) = joined.context("a webhook send panicked")?;
        match outcome {
            Ok(status) => {
                client
                    .execute(
                        "UPDATE webhooks.deliveries SET delivered_at = $2, last_status = $3, \
                         last_error = NULL WHERE id = $1",
                        &[&id, &now, &status],
                    )
                    .await?;
                report.delivered += 1;
            }
            Err((status, error, give_up)) => {
                let give_up = give_up || attempts >= MAX_ATTEMPTS;
                let next = now + backoff(attempts);
                client
                    .execute(
                        "UPDATE webhooks.deliveries SET last_status = $2, last_error = $3, \
                         next_attempt_at = $4, \
                         abandoned_at = CASE WHEN $5::boolean THEN $6::timestamptz END \
                         WHERE id = $1",
                        &[&id, &status, &error, &next, &give_up, &now],
                    )
                    .await?;
                if give_up {
                    report.abandoned += 1;
                } else {
                    report.retrying += 1;
                }
            }
        }
    }
    Ok(report)
}

/// One attempt: the response status on a 2xx, else the status (if any) and
/// why it failed.
async fn send(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    id: i64,
    event: &str,
    body: String,
) -> std::result::Result<i32, (Option<i32>, String)> {
    let signature = signature(&endpoint.secret, body.as_bytes());
    let response = http
        .post(&endpoint.url)
        .header("Content-Type", "application/json")
        .header("X-Aster-Event", event)
        .header("X-Aster-Delivery", id.to_string())
        .header("X-Aster-Signature", signature)
        .body(body)
        .send()
        .await
        .map_err(|error| (None, format!("{error:#}")))?;
    let status = i32::from(response.status().as_u16());
    if response.status().is_success() {
        Ok(status)
    } else {
        Err((Some(status), format!("the endpoint answered {status}")))
    }
}

/// Send due deliveries every [`INTERVAL`], when any endpoint is configured.
pub fn spawn(app: std::sync::Arc<App>) {
    if app.config.webhooks.is_empty() {
        return;
    }
    tokio::spawn(async move {
        loop {
            match deliver_once(&app, Utc::now()).await {
                Ok(report) if report == Report::default() => {}
                Ok(report) => crate::log::info(
                    "webhook deliveries",
                    json!({
                        "delivered": report.delivered,
                        "retrying": report.retrying,
                        "abandoned": report.abandoned,
                    }),
                ),
                Err(error) => crate::log::error(
                    "webhook deliveries failed",
                    json!({"error": format!("{error:#}")}),
                ),
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = r#"[{"name": "relay", "url": "https://hooks.example/aster",
        "secret": "0123456789abcdef", "events": ["test.quarantined", "test.broken"]}]"#;

    #[test]
    fn reads_endpoints_without_printing_secrets() {
        let endpoints = Endpoint::parse_all(ONE).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert!(endpoints[0].wants("test.broken"));
        assert!(!endpoints[0].wants("test.released"));
        assert!(!format!("{endpoints:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn refuses_bad_endpoints() {
        let with = |field: &str, value: &str| ONE.replace(field, value);
        for bad in [
            "{}".to_string(),
            with("https://hooks.example/aster", "http://hooks.example/aster"),
            with("https://hooks.example/aster", "ftp://x"),
            with("0123456789abcdef", "short"),
            with("\"test.broken\"", "\"test.flaky\""),
            with(r#"["test.quarantined", "test.broken"]"#, "[]"),
            with("\"relay\"", "\" \""),
            ONE.replace("]", "") + ", " + &ONE[1..],
            with("\"events\"", "\"extra\": 1, \"events\""),
        ] {
            assert!(Endpoint::parse_all(&bad).is_err(), "{bad}");
        }
        let twice = format!("[{0}, {0}]", &ONE[1..ONE.len() - 1]);
        assert!(Endpoint::parse_all(&twice).is_err());
    }

    #[test]
    fn plain_http_is_only_for_this_machine() {
        assert!(allowed_url("http://localhost:9000/hook"));
        assert!(allowed_url("http://127.0.0.1/hook"));
        assert!(allowed_url("http://[::1]:80/"));
        assert!(!allowed_url("http://localhost.evil.io/"));
        assert!(!allowed_url("http://10.0.0.1/"));
        assert!(allowed_url("https://10.0.0.1/"));
        assert!(!allowed_url("https://"));
    }

    #[test]
    fn signs_like_the_hmac_test_vectors() {
        // RFC 4231 test case 2.
        assert_eq!(
            signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn backs_off_by_doubling_up_to_six_hours() {
        assert_eq!(backoff(1).num_seconds(), 30);
        assert_eq!(backoff(2).num_seconds(), 60);
        assert_eq!(backoff(5).num_seconds(), 480);
        assert_eq!(backoff(10).num_seconds(), 30 << 9);
        assert_eq!(backoff(11).num_seconds(), 6 * 60 * 60);
        let waits: i64 = (1..MAX_ATTEMPTS).map(|a| backoff(a).num_seconds()).sum();
        assert_eq!(waits, 52_290);
        assert_eq!(backoff(1000).num_seconds(), 6 * 60 * 60);
    }
}
