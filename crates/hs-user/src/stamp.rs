//! [`Stamps`]: the change counter typing, receipts and presence stamp their records with, and
//! that a `/sync` token carries a cursor into (`SyncToken::typing_seq`, `receipts_seq`,
//! `presence_seq`).
//!
//! # Why the clock, not a plain counter
//!
//! These counters used to start at zero with every process. A client holding a token from before
//! a restart then carried a cursor of, say, 40, and the restarted process stamped its first
//! forty changes 1 to 40 -- every one of them "not newer than what you have", so the client saw
//! nobody typing, no receipt and no presence change until the counter caught up with its token.
//! Persisting receipts and presence would have made that worse, not better: a record stamped
//! before the restart would compare against a counter that had forgotten it.
//!
//! A stamp is therefore `max(previous + 1, microseconds since the Unix epoch)`: strictly
//! increasing within a process (the `previous + 1` half), and past every stamp an earlier process
//! issued once this one starts (the clock half), as long as the earlier process did not issue
//! more than a million stamps a second for long enough to run ahead of the clock by the length of
//! the restart. A record loaded from the store also raises the floor
//! ([`Stamps::observe`]), so nothing issued here can ever be older than something already held.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonic, restart-safe change counter. See the module docs.
#[derive(Debug, Default)]
pub struct Stamps {
    last: AtomicU64,
}

impl Stamps {
    /// A counter that has issued nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The next stamp: greater than every stamp this counter has issued or observed, and at least
    /// the current time in microseconds.
    pub fn next(&self) -> u64 {
        let now = now_micros();
        let mut current = self.last.load(Ordering::Acquire);
        loop {
            let next = now.max(current.saturating_add(1));
            match self.last.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return next,
                Err(actual) => current = actual,
            }
        }
    }

    /// Raises the floor to `seen` (a stamp read back from the store), so every later
    /// [`Stamps::next`] is greater than it.
    pub fn observe(&self, seen: u64) {
        self.last.fetch_max(seen, Ordering::AcqRel);
    }
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_strictly_increase_even_within_one_microsecond() {
        let stamps = Stamps::new();
        let mut previous = 0;
        for _ in 0..10_000 {
            let next = stamps.next();
            assert!(next > previous);
            previous = next;
        }
    }

    /// The restart property: a fresh counter (a new process) issues stamps past what an earlier
    /// one issued a moment ago, without being told anything.
    #[test]
    fn a_new_counter_starts_past_an_earlier_ones_stamps() {
        let before = Stamps::new();
        let old = (0..100).map(|_| before.next()).max().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let after = Stamps::new();
        assert!(after.next() > old);
    }

    #[test]
    fn an_observed_stamp_from_the_future_raises_the_floor() {
        let stamps = Stamps::new();
        let far = now_micros() + 60_000_000;
        stamps.observe(far);
        assert!(stamps.next() > far);
    }
}
