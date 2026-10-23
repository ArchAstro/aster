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
/// table is still full the newcomer shares an overflow bucket: attackers
/// that mint keys cannot grow memory, they only share limits.
const MAX_KEYS: usize = 100_000;
/// How many overflow buckets a limiter has. A newcomer lands in one by a
/// keyed hash (the seed is per limiter and per process, so a caller cannot
/// aim at a bucket), so a flood of minted keys has to drain all of them to
/// starve a real client, not one shared bucket.
const OVERFLOW_SHARDS: u64 = 256;
/// The table is swept for idle keys at most this often, so a flood of new
/// keys against a full table cannot make every request scan 100,000 entries.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// Exchanges running at once. The exchange holds one of the auth pool's
/// connections ([`AUTH_POOL_SIZE`]); a few more wait their turn.
pub const MAX_CONCURRENT_EXCHANGES: usize = 4;
/// Connections in the pool only `/v1/auth/token` uses.
pub const AUTH_POOL_SIZE: usize = 2;

struct Bucket {
    tokens: f64,
    last: Instant,
}

struct Table {
    buckets: HashMap<String, Bucket>,
    last_sweep: Option<Instant>,
    #[cfg(test)]
    sweeps: usize,
}

/// A token bucket per key: `per_minute` requests, refilled continuously.
///
/// Each limiter has its own table and its own overflow buckets, so keys
/// minted against one (client IPs, before authentication) never spend
/// another's (the authenticated subjects).
pub struct RateLimiter {
    per_minute: u32,
    table: Mutex<Table>,
    seed: std::collections::hash_map::RandomState,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            table: Mutex::new(Table {
                buckets: HashMap::new(),
                last_sweep: None,
                #[cfg(test)]
                sweeps: 0,
            }),
            seed: Default::default(),
        }
    }

    fn overflow_key(&self, key: &str) -> String {
        use std::hash::BuildHasher;
        format!("*overflow-{}*", self.seed.hash_one(key) % OVERFLOW_SHARDS)
    }

    /// Take one request from `key`'s bucket, or learn how many seconds
    /// until one is available.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let capacity = f64::from(self.per_minute);
        let rate = capacity / 60.0;
        let mut table = self.table.lock().unwrap();
        let mut key = key.to_string();
        if !table.buckets.contains_key(&key) && table.buckets.len() >= MAX_KEYS {
            let due = table
                .last_sweep
                .is_none_or(|at| now.saturating_duration_since(at) >= SWEEP_INTERVAL);
            if due {
                table.last_sweep = Some(now);
                #[cfg(test)]
                {
                    table.sweeps += 1;
                }
                table.buckets.retain(|_, b| {
                    b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * rate < capacity
                });
            }
            if table.buckets.len() >= MAX_KEYS {
                key = self.overflow_key(&key);
            }
        }
        let bucket = table.buckets.entry(key).or_insert(Bucket {
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
        self.table.lock().unwrap().buckets.len()
    }

    #[cfg(test)]
    fn sweeps(&self) -> usize {
        self.table.lock().unwrap().sweeps
    }
}

/// How many things one key may hold at once: a client IP's in-flight
/// requests or open connections. A held slot is a [`SlotGuard`]; the table
/// holds only keys with a live slot, so it is as small as the open work.
pub struct Concurrency {
    max: usize,
    held: Arc<Mutex<HashMap<String, usize>>>,
}

impl Concurrency {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            held: Arc::default(),
        }
    }

    /// A slot for `key`, or `None` when it already holds the maximum.
    pub fn try_acquire(&self, key: &str) -> Option<SlotGuard> {
        let mut held = self.held.lock().unwrap();
        let count = held.entry(key.to_string()).or_default();
        if *count >= self.max {
            return None;
        }
        *count += 1;
        Some(SlotGuard {
            key: key.to_string(),
            held: Arc::clone(&self.held),
        })
    }

    #[cfg(test)]
    fn keys(&self) -> usize {
        self.held.lock().unwrap().len()
    }
}

/// One slot of a [`Concurrency`], released on drop.
pub struct SlotGuard {
    key: String,
    held: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut held = self.held.lock().unwrap();
        if let Some(count) = held.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.key);
            }
        }
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
    /// Large request bodies (a cache commit, a test-results upload) read at
    /// once: each can weigh `ASTER_SRV_MAX_BODY_BYTES` in memory.
    pub large_bodies: Arc<Semaphore>,
    /// Requests in flight, per client IP ([`ip_key`]).
    pub requests_per_ip: Concurrency,
    pub trusted_proxy_hops: usize,
}

impl Limits {
    pub fn new(config: &Config) -> Self {
        Self {
            token_ip: RateLimiter::new(config.rate_token_ip_per_min),
            api_ip: RateLimiter::new(config.rate_api_ip_per_min),
            api_subject: RateLimiter::new(config.rate_api_subject_per_min),
            exchanges: Arc::new(Semaphore::new(MAX_CONCURRENT_EXCHANGES)),
            large_bodies: Arc::new(Semaphore::new(config.max_large_bodies)),
            requests_per_ip: Concurrency::new(config.max_connections_per_ip),
            trusted_proxy_hops: config.trusted_proxy_hops,
        }
    }
}

/// The `X-Forwarded-For` value of a request: every header line of that name,
/// in order, joined into one list (RFC 9110: repeated lines are one
/// comma-separated list). A line that is not text is skipped. `None` when
/// there is nothing to read.
pub fn forwarded_for<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let joined = lines.into_iter().collect::<Vec<_>>().join(",");
    (!joined.is_empty()).then_some(joined)
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

    /// A limiter whose table is full of spent buckets, none idle.
    fn full_limiter(per_minute: u32, now: Instant) -> RateLimiter {
        let limiter = RateLimiter::new(per_minute);
        for n in 0..MAX_KEYS {
            for _ in 0..per_minute {
                limiter.check_at(&format!("k{n}"), now).unwrap();
            }
        }
        assert_eq!(limiter.len(), MAX_KEYS);
        limiter
    }

    /// N7: the table stays bounded, and a flood of minted keys that fills it
    /// does not take a real newcomer's whole allowance: the overflow is many
    /// buckets chosen by a keyed hash, not one shared bucket.
    #[test]
    fn a_flood_of_minted_keys_does_not_starve_a_real_newcomer() {
        let now = Instant::now();
        let limiter = full_limiter(60, now);
        // 1,000 minted keys arrive at once. One shared bucket of 60 would be
        // empty after 60 of them; 256 of them leave each about 4 spent.
        for n in 0..1000 {
            let _ = limiter.check_at(&format!("mint-{n}"), now);
        }
        assert!(limiter.len() <= MAX_KEYS + OVERFLOW_SHARDS as usize);
        // A real client arriving now still has its allowance, and the same
        // client lands in the same overflow bucket each time.
        for _ in 0..40 {
            assert!(limiter.check_at("real-client", now).is_ok());
        }
        // Another limiter's keys are never affected.
        let other = RateLimiter::new(60);
        assert!(other.check_at("real-client", now).is_ok());
        // Later, idle buckets are dropped and real keys come back with a
        // table of their own.
        let later = now + Duration::from_secs(120);
        assert!(limiter.check_at("new-3", later).is_ok());
        assert!(limiter.len() < 10);
    }

    /// N7: a full table is swept for idle keys at most once per interval,
    /// however many new keys arrive, so a flood cannot make each request
    /// scan the whole table.
    #[test]
    fn a_full_table_is_swept_at_most_once_per_interval() {
        let now = Instant::now();
        let limiter = full_limiter(1, now);
        for n in 0..5000 {
            let _ = limiter.check_at(&format!("mint-{n}"), now + Duration::from_millis(n % 900));
        }
        assert_eq!(limiter.sweeps(), 1);
        let _ = limiter.check_at("later", now + Duration::from_millis(1500));
        assert_eq!(limiter.sweeps(), 2);
    }

    /// N7: the subject limiter, keyed by verified subjects, is a different
    /// table from the per-IP limiters minted keys fill, so an unauthenticated
    /// flood cannot spend an authenticated subject's allowance.
    #[test]
    fn a_flood_against_the_ip_limiter_leaves_subjects_untouched() {
        let config = crate::Config::from_env(|name| match name {
            "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
            "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
            _ => None,
        })
        .unwrap();
        let limits = Limits::new(&config);
        let now = Instant::now();
        for n in 0..(MAX_KEYS + 5000) {
            let _ = limits.api_ip.check_at(&format!("203.0.{n}"), now);
        }
        for _ in 0..100 {
            assert!(limits.api_subject.check_at("ci:ArchAstro/x", now).is_ok());
        }
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
    fn a_key_holds_at_most_its_cap_and_a_dropped_slot_frees_it() {
        let slots = Concurrency::new(2);
        let a = slots.try_acquire("a").unwrap();
        let _a2 = slots.try_acquire("a").unwrap();
        assert!(slots.try_acquire("a").is_none());
        // Another key is untouched.
        let b = slots.try_acquire("b").unwrap();
        drop(a);
        assert!(slots.try_acquire("a").is_some());
        // The table keeps only keys with a live slot.
        drop(b);
        assert_eq!(slots.keys(), 1);
    }

    /// N12: a client may send any number of `X-Forwarded-For` lines; they
    /// are one list, in order, and the load balancer's entries are the last.
    #[test]
    fn repeated_forwarded_for_lines_are_one_list_and_cannot_move_the_client() {
        let peer = ip("10.0.0.9");
        let lb = "203.0.113.7, 34.8.0.1";
        // The caller's own lines come before the balancer's.
        let one_line = forwarded_for(["6.6.6.6, 7.7.7.7, 203.0.113.7, 34.8.0.1"]);
        let two_lines = forwarded_for(["6.6.6.6", "7.7.7.7", lb]);
        let spoof_first = forwarded_for(["6.6.6.6, 5.5.5.5", lb]);
        for header in [&one_line, &two_lines, &spoof_first] {
            assert_eq!(client_ip(header.as_deref(), peer, 1), ip("203.0.113.7"));
        }
        // Lines are joined in order, so hops walk across a line boundary.
        assert_eq!(forwarded_for(["a", "b, c"]).as_deref(), Some("a,b, c"));
        assert_eq!(
            client_ip(
                forwarded_for(["9.9.9.9", "203.0.113.7", "34.8.0.1"]).as_deref(),
                peer,
                1
            ),
            ip("203.0.113.7")
        );
        // No lines, or empty ones: the peer.
        assert_eq!(forwarded_for([]), None);
        assert_eq!(forwarded_for([""]), None);
        assert_eq!(client_ip(forwarded_for([]).as_deref(), peer, 1), peer);
        // A spoofed line added after the balancer's entries takes the
        // trusted position: the balancer must be the last to append, which
        // is why the pods must be reachable through it alone.
        assert_eq!(
            client_ip(forwarded_for([lb, "6.6.6.6"]).as_deref(), peer, 1),
            ip("34.8.0.1")
        );
    }

    /// N12: each trusted hop moves the client one entry left, and an entry
    /// that is not an address, or a header too short for the hops, falls back
    /// to the peer.
    #[test]
    fn the_client_moves_left_one_entry_per_trusted_hop() {
        let peer = ip("10.0.0.9");
        let header = forwarded_for(["1.1.1.1, 2.2.2.2, 3.3.3.3, 4.4.4.4"]);
        let at = |hops| client_ip(header.as_deref(), peer, hops);
        assert_eq!(at(0), peer);
        assert_eq!(at(1), ip("3.3.3.3"));
        assert_eq!(at(2), ip("2.2.2.2"));
        assert_eq!(at(3), ip("1.1.1.1"));
        assert_eq!(at(4), peer);
        assert_eq!(
            client_ip(Some("1.1.1.1, not-an-ip, 4.4.4.4"), peer, 1),
            peer
        );
        assert_eq!(client_ip(Some("1.1.1.1,,4.4.4.4"), peer, 1), peer);
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
