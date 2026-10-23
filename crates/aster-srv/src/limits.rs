//! Request limits and the client IP they are keyed on.
//!
//! Limits are in-memory token buckets, **per replica**: with N replicas an
//! attacker gets N times the rate. They are the second layer. The global,
//! cross-replica limit is Cloud Armor on the load balancer (infra PR B);
//! these stop one replica from being driven into the ground, and they key on
//! the verified subject as well as the IP, which Cloud Armor cannot.
//!
//! ## Client IP and the trust model
//!
//! Behind Google's external HTTP(S) load balancer, `X-Forwarded-For` is
//! `<whatever the client sent>, <client IP>, <load balancer IP>`: the balancer
//! appends the address it saw and its own. Everything left of those two
//! entries is attacker-controlled, so [`client_ip`] counts from the right and
//! skips `ASTER_SRV_TRUSTED_PROXY_HOPS` entries (default 1: the balancer's
//! own address), taking the next one: the right-most address that is not
//! ours. A header with too few entries, or an unparsable one, falls back to
//! the TCP peer. With hops 0 the header is ignored.
//!
//! This is only sound while the pod is reachable through the balancer alone:
//! a caller that reaches the pod directly can write the whole header. The
//! deployment's network policy must keep it that way; the worst case
//! otherwise is bypassing per-IP limits (the per-subject limit still holds
//! for authenticated routes).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::Config;

/// Most keys a limiter tracks. Past it, idle keys are dropped, and if the
/// table is still full the newcomer shares one overflow bucket: attackers
/// that mint keys cannot grow memory, they only share a limit.
const MAX_KEYS: usize = 100_000;
const OVERFLOW_KEY: &str = "*overflow*";
/// Exchanges running at once. The exchange holds one of the auth pool's
/// connections ([`AUTH_POOL_SIZE`]); a few more wait their turn.
pub const MAX_CONCURRENT_EXCHANGES: usize = 4;
/// Connections in the pool only `/v1/auth/token` uses.
pub const AUTH_POOL_SIZE: usize = 2;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A token bucket per key: `per_minute` requests, refilled continuously.
pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one request from `key`'s bucket, or learn how many seconds
    /// until one is available.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let capacity = f64::from(self.per_minute);
        let rate = capacity / 60.0;
        let mut buckets = self.buckets.lock().unwrap();
        let mut key = key;
        if !buckets.contains_key(key) && buckets.len() >= MAX_KEYS {
            buckets.retain(|_, b| {
                b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * rate < capacity
            });
            if buckets.len() >= MAX_KEYS {
                key = OVERFLOW_KEY;
            }
        }
        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - bucket.tokens) / rate).ceil().max(1.0) as u64)
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

/// Every limit one replica applies.
pub struct Limits {
    /// `POST /v1/auth/token`, per client IP.
    pub token_ip: RateLimiter,
    /// All of `/v1/*`, per client IP.
    pub api_ip: RateLimiter,
    /// Authenticated `/v1/*`, per subject.
    pub api_subject: RateLimiter,
    /// Exchanges allowed to run at once.
    pub exchanges: Arc<Semaphore>,
    pub trusted_proxy_hops: usize,
}

impl Limits {
    pub fn new(config: &Config) -> Self {
        Self {
            token_ip: RateLimiter::new(config.rate_token_ip_per_min),
            api_ip: RateLimiter::new(config.rate_api_ip_per_min),
            api_subject: RateLimiter::new(config.rate_api_subject_per_min),
            exchanges: Arc::new(Semaphore::new(MAX_CONCURRENT_EXCHANGES)),
            trusted_proxy_hops: config.trusted_proxy_hops,
        }
    }
}

/// The client address: see the module docs for the trust model.
pub fn client_ip(forwarded_for: Option<&str>, peer: IpAddr, trusted_hops: usize) -> IpAddr {
    let from_header = || {
        let entries: Vec<&str> = forwarded_for?.split(',').map(str::trim).collect();
        let index = entries.len().checked_sub(trusted_hops + 1)?;
        entries[index].parse::<IpAddr>().ok()
    };
    let ip = if trusted_hops == 0 {
        None
    } else {
        from_header()
    };
    ip.unwrap_or(peer).to_canonical()
}

/// The limiter key for an address: IPv6 clients are grouped by /64, the
/// smallest block a host or household is given.
pub fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// Seconds as a `Retry-After` value.
pub fn retry_after(seconds: u64) -> String {
    seconds.min(Duration::from_secs(3600).as_secs()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_bucket_allows_its_burst_then_refills_over_time() {
        let limiter = RateLimiter::new(60);
        let start = Instant::now();
        for _ in 0..60 {
            assert!(limiter.check_at("a", start).is_ok());
        }
        assert_eq!(limiter.check_at("a", start), Err(1));
        // Another key is untouched.
        assert!(limiter.check_at("b", start).is_ok());
        // One token per second at 60 a minute.
        assert!(limiter
            .check_at("a", start + Duration::from_millis(1100))
            .is_ok());
        assert!(limiter
            .check_at("a", start + Duration::from_millis(1100))
            .is_err());
        // A slow limit reports a long wait.
        let slow = RateLimiter::new(10);
        for _ in 0..10 {
            slow.check_at("x", start).unwrap();
        }
        assert_eq!(slow.check_at("x", start), Err(6));
    }

    #[test]
    fn the_table_is_bounded_and_overflow_keys_share_one_bucket() {
        let limiter = RateLimiter::new(1);
        let now = Instant::now();
        for n in 0..MAX_KEYS {
            limiter.check_at(&format!("k{n}"), now).unwrap();
        }
        assert_eq!(limiter.len(), MAX_KEYS);
        // Every bucket is spent, so none is idle to drop: newcomers share.
        assert!(limiter.check_at("new-1", now).is_ok());
        assert!(limiter.check_at("new-2", now).is_err());
        assert_eq!(limiter.len(), MAX_KEYS + 1);
        // Later, idle buckets are dropped and real keys come back.
        let later = now + Duration::from_secs(120);
        assert!(limiter.check_at("new-3", later).is_ok());
        assert!(limiter.len() < 10);
    }

    #[test]
    fn the_client_is_the_right_most_address_we_do_not_own() {
        let peer = ip("10.0.0.9");
        // Google appends `<client>, <load balancer>`; the left is the caller's.
        let header = "6.6.6.6, 203.0.113.7, 34.8.0.1";
        assert_eq!(client_ip(Some(header), peer, 1), ip("203.0.113.7"));
        // A spoofed prefix changes nothing.
        let spoofed = "1.1.1.1, 2.2.2.2, 6.6.6.6, 203.0.113.7, 34.8.0.1";
        assert_eq!(client_ip(Some(spoofed), peer, 1), ip("203.0.113.7"));
        // Two trusted hops (a proxy in front of the balancer).
        assert_eq!(client_ip(Some(header), peer, 2), ip("6.6.6.6"));
        // Too few entries, no header, junk, or hops 0: the TCP peer.
        assert_eq!(client_ip(Some("34.8.0.1"), peer, 1), peer);
        assert_eq!(client_ip(None, peer, 1), peer);
        assert_eq!(client_ip(Some("junk, 34.8.0.1"), peer, 1), peer);
        assert_eq!(client_ip(Some(header), peer, 0), peer);
        assert_eq!(client_ip(Some(header), peer, 5), peer);
        // IPv6 and mapped IPv4 normalise.
        assert_eq!(
            client_ip(Some("2001:db8::1, 34.8.0.1"), peer, 1),
            ip("2001:db8::1")
        );
        assert_eq!(
            client_ip(Some("::ffff:203.0.113.7, 34.8.0.1"), peer, 1),
            ip("203.0.113.7")
        );
    }

    #[test]
    fn ipv6_clients_share_a_limit_per_64() {
        assert_eq!(
            ip_key(ip("2001:db8:1:2:aaaa::1")),
            ip_key(ip("2001:db8:1:2:bbbb::9"))
        );
        assert_ne!(ip_key(ip("2001:db8:1:2::1")), ip_key(ip("2001:db8:1:3::1")));
        assert_eq!(ip_key(ip("203.0.113.7")), "203.0.113.7");
    }
}
