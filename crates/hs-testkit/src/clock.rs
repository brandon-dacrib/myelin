//! A fake clock compatible with `tokio::time`'s paused-time mode.
//!
//! [`FakeClock`] does not maintain its own independent counter the way a naive test double would;
//! it reads `tokio::time::Instant::now()` and offsets it from a fixed Unix-epoch start. That means
//! it advances exactly when `tokio::time::advance` (or a timer firing under paused time) advances,
//! which is what makes it "compatible with tokio's paused time" rather than a second, competing
//! notion of time that a test has to keep in sync by hand. Use it with `#[tokio::test(start_paused
//! = true)]` (or a manual `tokio::time::pause()`); under real time it also works, but then it is
//! just an expensive way to read the wall clock.
//!
//! Crates with their own `Clock` trait (`hs-auth::clock::Clock`, for one) adapt [`FakeClock`] with
//! a one-line wrapper in their own test code; this type intentionally has no dependency on any of
//! them so `hs-testkit` stays usable by every track without pulling their crates in.

use std::time::Duration;

use tokio::time::Instant;

/// A millisecond-resolution clock whose value is `start_unix_ms` plus however much virtual time
/// has elapsed since this clock was created, as measured by `tokio::time::Instant`.
///
/// Under `tokio::time::pause()`, virtual time only moves when the runtime is told to move it
/// (`tokio::time::advance`, or a sleeping task's timer firing and the executor running out of
/// other work) — so a test using [`FakeClock`] gets full control without sprinkling real `sleep`
/// calls through its scenario.
#[derive(Debug, Clone, Copy)]
pub struct FakeClock {
    start_unix_ms: u64,
    start_instant: Instant,
}

impl FakeClock {
    /// A clock that reads `start_unix_ms` right now, and moves forward in lockstep with
    /// `tokio::time`'s clock from this point on.
    #[must_use]
    pub fn new(start_unix_ms: u64) -> Self {
        Self {
            start_unix_ms,
            start_instant: Instant::now(),
        }
    }

    /// The current time, milliseconds since the Unix epoch, as of the last time `tokio::time`
    /// moved forward.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        let elapsed = Instant::now().saturating_duration_since(self.start_instant);
        self.start_unix_ms
            .saturating_add(elapsed.as_millis() as u64)
    }

    /// Advances tokio's paused clock by `duration`, firing any timers that fall due. Panics (via
    /// `tokio::time::advance`) if called without paused time in effect; pair with
    /// `#[tokio::test(start_paused = true)]`.
    pub async fn advance(&self, duration: Duration) {
        tokio::time::advance(duration).await;
    }

    /// [`FakeClock::advance`] by a whole number of milliseconds; the common case in scenarios
    /// asserting token or session expiry.
    pub async fn advance_ms(&self, ms: u64) {
        self.advance(Duration::from_millis(ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn now_ms_advances_exactly_with_paused_tokio_time() {
        let clock = FakeClock::new(1_700_000_000_000);
        let start = clock.now_ms();
        assert_eq!(start, 1_700_000_000_000);

        clock.advance_ms(5_000).await;
        assert_eq!(clock.now_ms(), 1_700_000_005_000);

        clock.advance(Duration::from_secs(60)).await;
        assert_eq!(clock.now_ms(), 1_700_000_065_000);
    }

    #[tokio::test(start_paused = true)]
    async fn two_clocks_created_at_different_moments_stay_in_sync_with_tokio_time() {
        let a = FakeClock::new(0);
        a.advance_ms(1_000).await;
        let b = FakeClock::new(500);
        // `b` was created after one second of virtual time already passed, with its own base;
        // both must move identically from here on.
        a.advance_ms(2_000).await;
        assert_eq!(a.now_ms(), 3_000);
        assert_eq!(b.now_ms(), 2_500);
    }
}
