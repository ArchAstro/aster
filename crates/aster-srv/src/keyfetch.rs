//! When a verifier may refetch its issuer's signing keys.
//!
//! A token that names a key the verifier has not seen is either a real key
//! rotation (the key is in the issuer's JWK set) or a probe (it is not).
//! The verifier cannot tell before it fetches, and fetching on every unknown
//! key would let a caller turn requests into fetches against the issuer.
//!
//! The rules, shared by the GitHub OIDC verifier ([`crate::auth`]) and the
//! IAP verifier ([`crate::iap`]):
//!
//! 1. A fetch of any outcome, success or failure, is followed by at least
//!    [`RefetchPolicy::min_interval`] (10 seconds) before the next one. Every
//!    unknown key in that window is refused without a fetch, so a flood of
//!    random keys costs the issuer at most one fetch per interval.
//! 2. A key a successful fetch did not find is remembered for
//!    [`RefetchPolicy::absent_for`] (60 seconds): the same unknown key does
//!    not trigger another fetch inside that time, however often it arrives.
//! 3. Neither a failed fetch nor a probe for a *different* key blocks a real
//!    rotation for more than `min_interval`. Before this, the attempt time
//!    was recorded before the fetch, so one failed fetch or one probe held
//!    off a real rotation for a full minute.
//!
//! Callers that arrive while a fetch runs wait for it (the state is behind
//! one lock held across the fetch) and share its result.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The refetch timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefetchPolicy {
    /// The least time between two fetches, whatever their outcome.
    pub min_interval: Duration,
    /// How long a key a successful fetch did not find is not looked for again.
    pub absent_for: Duration,
}

impl Default for RefetchPolicy {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(10),
            absent_for: Duration::from_secs(60),
        }
    }
}

/// Unknown keys remembered at once. A flood of distinct random keys cannot
/// grow this: past the cap the oldest entries are dropped.
const MAX_ABSENT: usize = 64;

/// What the verifier remembers between fetches. Lives behind the lock that
/// serialises fetches.
#[derive(Default)]
pub struct RefetchState {
    last_attempt: Option<Instant>,
    absent: HashMap<String, Instant>,
}

impl RefetchState {
    /// Whether a token naming `kid`, which is not in the key set, may cause
    /// a fetch now.
    pub fn may_fetch(&self, kid: &str, now: Instant, policy: RefetchPolicy) -> bool {
        let since = |at: Instant| now.saturating_duration_since(at);
        if self
            .last_attempt
            .is_some_and(|at| since(at) < policy.min_interval)
        {
            return false;
        }
        self.absent
            .get(kid)
            .is_none_or(|at| since(*at) >= policy.absent_for)
    }

    /// A fetch starts now. Recorded before the request so that a fetch that
    /// fails still spaces the next one.
    pub fn attempted(&mut self, now: Instant) {
        self.last_attempt = Some(now);
    }

    /// A fetch succeeded and its key set held `kid`: forget any record that
    /// it was absent.
    pub fn found(&mut self, kid: &str) {
        self.absent.remove(kid);
    }

    /// A fetch succeeded and its key set did not hold `kid`.
    pub fn missing(&mut self, kid: &str, now: Instant) {
        if self.absent.len() >= MAX_ABSENT && !self.absent.contains_key(kid) {
            let oldest = self
                .absent
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(kid, _)| kid.clone());
            if let Some(oldest) = oldest {
                self.absent.remove(&oldest);
            }
        }
        self.absent.insert(kid.to_string(), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: RefetchPolicy = RefetchPolicy {
        min_interval: Duration::from_secs(10),
        absent_for: Duration::from_secs(60),
    };

    #[test]
    fn a_first_unknown_key_fetches() {
        assert!(RefetchState::default().may_fetch("k", Instant::now(), POLICY));
    }

    #[test]
    fn a_failed_fetch_blocks_the_next_for_the_short_interval_only() {
        let start = Instant::now();
        let mut state = RefetchState::default();
        state.attempted(start);
        // No success was recorded: the real rotation waits ten seconds, not a minute.
        assert!(!state.may_fetch("rotated", start + Duration::from_secs(9), POLICY));
        assert!(state.may_fetch("rotated", start + Duration::from_secs(10), POLICY));
    }

    #[test]
    fn a_probe_blocks_itself_for_a_minute_and_a_different_key_for_ten_seconds() {
        let start = Instant::now();
        let mut state = RefetchState::default();
        state.attempted(start);
        state.missing("probe", start);
        let later = |s| start + Duration::from_secs(s);
        assert!(!state.may_fetch("probe", later(30), POLICY));
        assert!(state.may_fetch("rotated", later(10), POLICY));
        assert!(state.may_fetch("probe", later(60), POLICY));
    }

    #[test]
    fn a_key_found_later_is_forgotten_as_absent() {
        let start = Instant::now();
        let mut state = RefetchState::default();
        state.missing("k", start);
        state.found("k");
        assert!(state.may_fetch("k", start, POLICY));
    }

    #[test]
    fn remembered_absent_keys_are_bounded() {
        let start = Instant::now();
        let mut state = RefetchState::default();
        for n in 0..1000u64 {
            state.missing(&format!("k{n}"), start + Duration::from_millis(n));
        }
        assert_eq!(state.absent.len(), MAX_ABSENT);
        // The newest are kept, the oldest dropped.
        assert!(state.absent.contains_key("k999"));
        assert!(!state.absent.contains_key("k0"));
    }
}
