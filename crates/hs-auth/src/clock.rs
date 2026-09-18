//! A trivial clock abstraction so tests can control time instead of racing the wall clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of "now", in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    /// The current time, milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// The real wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock tests can set and advance deterministically.
#[derive(Debug)]
pub struct FixedClock(AtomicU64);

impl FixedClock {
    /// A clock starting at `now_ms`.
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }

    /// Advances the clock by `ms` milliseconds.
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// Sets the clock to an absolute time.
    pub fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_plausible() {
        let now = SystemClock.now_ms();
        // After 2026-01-01 in epoch millis.
        assert!(now > 1_767_225_600_000);
    }

    #[test]
    fn fixed_clock_advances() {
        let clock = FixedClock::new(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        clock.advance(500);
        assert_eq!(clock.now_ms(), 1_500);
        clock.set(9_999);
        assert_eq!(clock.now_ms(), 9_999);
    }
}
