//! The order in which one invocation's ready targets take execution slots.
//!
//! Every ready target runs on its own thread, and each thread polls the host
//! locks until a slot is free. Left alone, whichever thread polls first after
//! a slot frees wins, so a target that gates a long chain of dependents can
//! sit behind targets that gate nothing. A [`Gate`] gives the waiting threads
//! a line: the target with the longest remaining chain of dependents goes
//! first, and requests with equal chains go in the order they were made.
//!
//! The line orders one invocation's own targets. Other invocations on the
//! host still compete through the OS locks.

#[cfg(unix)]
use std::cmp::Reverse;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
#[cfg(unix)]
use std::time::Duration;

/// Waiting targets of one invocation.
#[derive(Default)]
pub(crate) struct Gate {
    waiting: Mutex<Waiting>,
    changed: Condvar,
}

#[derive(Default)]
struct Waiting {
    next: u64,
    entries: Vec<Entry>,
}

struct Entry {
    seq: u64,
    /// Targets on the longest chain that starts at this one, itself included.
    #[cfg_attr(not(unix), allow(dead_code))]
    chain: usize,
    /// Held up by something other than the slots (an exclusive resource), so
    /// waiters behind it need not wait for it.
    parked: bool,
}

impl Entry {
    /// Smaller sorts first: longer chain, then earlier request.
    #[cfg(unix)]
    fn priority(&self) -> (Reverse<usize>, u64) {
        (Reverse(self.chain), self.seq)
    }
}

impl Gate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Join the line. Call from the thread that launches targets, in launch
    /// order, before the target's own thread starts: requests with the same
    /// chain length then keep that order.
    pub(crate) fn enter(self: &Arc<Self>, chain: usize) -> Ticket {
        let mut waiting = self.lock();
        let seq = waiting.next;
        waiting.next += 1;
        waiting.entries.push(Entry {
            seq,
            chain,
            parked: false,
        });
        Ticket {
            gate: Arc::clone(self),
            seq,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A place in the line. Dropping it leaves the line.
pub(crate) struct Ticket {
    gate: Arc<Gate>,
    seq: u64,
}

impl Ticket {
    fn with_entry(&self, change: impl FnOnce(&mut Entry)) {
        let mut waiting = self.gate.lock();
        if let Some(entry) = waiting.entries.iter_mut().find(|e| e.seq == self.seq) {
            change(entry);
        }
        drop(waiting);
        self.gate.changed.notify_all();
    }

    /// Stop holding up the targets behind this one while something other
    /// than a free slot is awaited.
    pub(crate) fn park(&self) {
        self.with_entry(|entry| entry.parked = true);
    }

    /// Hold up the targets behind this one again.
    pub(crate) fn unpark(&self) {
        self.with_entry(|entry| entry.parked = false);
    }

    /// Whether it is this target's turn to try for a slot, waiting up to
    /// `limit` for it to become so. It is not while a waiter ahead of it
    /// (longer chain, or the same chain requested earlier) still wants a slot.
    #[cfg(unix)]
    pub(crate) fn wait_turn(&self, limit: Duration) -> bool {
        let waiting = self.gate.lock();
        let (waiting, _) = self
            .gate
            .changed
            .wait_timeout_while(waiting, limit, |waiting| !self.is_turn(waiting))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.is_turn(&waiting)
    }

    #[cfg(unix)]
    fn is_turn(&self, waiting: &Waiting) -> bool {
        let Some(mine) = waiting.entries.iter().find(|e| e.seq == self.seq) else {
            return true;
        };
        !waiting
            .entries
            .iter()
            .any(|other| !other.parked && other.priority() < mine.priority())
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut waiting = self.gate.lock();
        waiting.entries.retain(|e| e.seq != self.seq);
        drop(waiting);
        self.gate.changed.notify_all();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const NOW: Duration = Duration::ZERO;

    #[test]
    fn the_longest_chain_goes_first_whatever_the_request_order() {
        let gate = Gate::new();
        let independent = gate.enter(1);
        let gating = gate.enter(3);
        let middle = gate.enter(2);
        assert!(!independent.wait_turn(NOW));
        assert!(!middle.wait_turn(NOW));
        assert!(gating.wait_turn(NOW));

        drop(gating);
        assert!(!independent.wait_turn(NOW));
        assert!(middle.wait_turn(NOW));

        drop(middle);
        assert!(independent.wait_turn(NOW));
    }

    #[test]
    fn equal_chains_go_in_request_order() {
        let gate = Gate::new();
        let first = gate.enter(1);
        let second = gate.enter(1);
        let third = gate.enter(1);
        assert!(first.wait_turn(NOW));
        assert!(!second.wait_turn(NOW));
        assert!(!third.wait_turn(NOW));
        drop(first);
        assert!(second.wait_turn(NOW));
        assert!(!third.wait_turn(NOW));
    }

    #[test]
    fn a_target_waiting_on_a_resource_does_not_hold_up_the_rest() {
        let gate = Gate::new();
        let blocked = gate.enter(5);
        let other = gate.enter(1);
        assert!(!other.wait_turn(NOW));
        blocked.park();
        assert!(other.wait_turn(NOW));
        // Parked, it still waits for any waiter that is ahead and not parked.
        let ahead = gate.enter(9);
        assert!(!blocked.wait_turn(NOW));
        drop(ahead);
        blocked.unpark();
        assert!(blocked.wait_turn(NOW));
        assert!(!other.wait_turn(NOW));
    }

    #[test]
    fn a_waiter_wakes_when_the_one_ahead_leaves() {
        let gate = Gate::new();
        let ahead = gate.enter(2);
        let behind = gate.enter(1);
        let waiter = std::thread::spawn(move || behind.wait_turn(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(50));
        drop(ahead);
        assert!(waiter.join().unwrap());
    }
}
