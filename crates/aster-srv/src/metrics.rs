//! Prometheus counters, served at `/metrics` on the main port only.
//!
//! One [`Metrics`] per [`crate::App`]. Label values are fixed strings chosen
//! in code (route templates and reason codes), never request text, so a
//! caller cannot grow the series set.
//!
//! | counter | labels |
//! |---------|--------|
//! | `aster_srv_auth_failures_total` | `route`, `reason` |
//! | `aster_srv_person_tokens_issued_total` | none |
//! | `aster_srv_rate_limited_total` | `route`, `scope` |
//!
//! Counts are per replica and reset when it restarts; use `rate()` and `sum`.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Default)]
pub struct Metrics {
    auth_failures: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
    rate_limited: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
    person_tokens_issued: AtomicU64,
}

impl Metrics {
    pub fn auth_failure(&self, route: &'static str, reason: &'static str) {
        *self
            .auth_failures
            .lock()
            .unwrap()
            .entry((route, reason))
            .or_default() += 1;
    }

    pub fn rate_limited(&self, route: &'static str, scope: &'static str) {
        *self
            .rate_limited
            .lock()
            .unwrap()
            .entry((route, scope))
            .or_default() += 1;
    }

    pub fn person_token_issued(&self) {
        self.person_tokens_issued.fetch_add(1, Ordering::Relaxed);
    }

    /// The Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let labelled = |out: &mut String,
                        name: &str,
                        help: &str,
                        keys: (&str, &str),
                        map: &BTreeMap<(&'static str, &'static str), u64>| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            for ((a, b), count) in map {
                let _ = writeln!(
                    out,
                    "{name}{{{}=\"{a}\",{}=\"{b}\"}} {count}",
                    keys.0, keys.1
                );
            }
        };
        labelled(
            &mut out,
            "aster_srv_auth_failures_total",
            "Requests refused for authentication or authorization, by route and reason.",
            ("route", "reason"),
            &self.auth_failures.lock().unwrap(),
        );
        let _ = writeln!(
            out,
            "# HELP aster_srv_person_tokens_issued_total Person tokens minted at sign-in.\n\
             # TYPE aster_srv_person_tokens_issued_total counter\n\
             aster_srv_person_tokens_issued_total {}",
            self.person_tokens_issued.load(Ordering::Relaxed)
        );
        labelled(
            &mut out,
            "aster_srv_rate_limited_total",
            "Requests refused with 429 or 503 by a limit, by route and scope.",
            ("route", "scope"),
            &self.rate_limited.lock().unwrap(),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_in_the_prometheus_text_format() {
        let metrics = Metrics::default();
        metrics.auth_failure("/v1/auth/token", "invalid_grant");
        metrics.auth_failure("/v1/auth/token", "invalid_grant");
        metrics.person_token_issued();
        metrics.rate_limited("/v1/cache", "ip");
        let text = metrics.render();
        assert!(text.contains(
            "aster_srv_auth_failures_total{route=\"/v1/auth/token\",reason=\"invalid_grant\"} 2\n"
        ));
        assert!(text.contains("aster_srv_person_tokens_issued_total 1\n"));
        assert!(text.contains("aster_srv_rate_limited_total{route=\"/v1/cache\",scope=\"ip\"} 1\n"));
        assert!(text.contains("# TYPE aster_srv_auth_failures_total counter"));
    }
}
