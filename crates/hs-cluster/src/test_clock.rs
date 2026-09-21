//! Clock helpers for this crate's paused-clock tests (`#[tokio::test(start_paused = true)]`).
//!
//! `tests/chaos.rs` keeps its own copy of these, because an integration test compiles against the
//! crate from the outside and cannot see anything behind `#[cfg(test)]`. The two must agree, and
//! the reason they must is written out in [`settle`].

use std::time::Duration;

/// The most real time one [`settle`] round will spend waiting for the blocking pool. See
/// [`settle`] for why there is a ratio at all and why it is capped.
const MAX_REAL_TIME_PER_ROUND: Duration = Duration::from_millis(5);

/// Advances the paused virtual clock by `step`, `rounds` times, letting every background task --
/// the async ones and the ones parked on a `spawn_blocking` join -- make progress in between.
///
/// That second group is the part that is easy to leave out, and expensive to leave out. Yielding
/// under a paused clock runs *async* tasks, but this crate's heartbeat, failure detection and
/// shard acquisition all finish on `spawn_blocking` threads, which need real wall-clock time that
/// `yield_now` does not cost. A round that advances virtual time while granting no real time lets
/// the clock outrun the work it is supposed to be pacing: the background loop falls a little
/// further behind every round, and how far it gets stops being a property of the code and becomes
/// a property of how fast the machine is.
///
/// That is exactly how `a_partitioned_replica_cannot_write_after_being_fenced` came to pass on
/// every developer laptop and every amd64 run and fail on the slower arm64 runner. Nothing was
/// racing: the takeover happens reliably once the survivor's loop is allowed to run. The test had
/// simply travelled 600ms of virtual time -- four times the lease TTL, so apparently generous --
/// while handing the blocking pool about 30ms of real time in which to do the work that virtual
/// time was pretending had already happened.
///
/// So every round hands the blocking pool `step / 4` of real time, capped at
/// [`MAX_REAL_TIME_PER_ROUND`] so the fixed-round driver loops stay cheap. The ratio is the point,
/// not the absolute figure: virtual time never runs more than about four times ahead of the wall
/// clock, which is what makes the round counts below a property of the code rather than of the
/// host. Measured against a deliberately slowed store, the failover these loops pace needs 10-17
/// rounds here and still lands in 53 on a host slow enough to spend 20ms inside every store call
/// -- where the same loop without the real-time grant never gets there at all.
pub(crate) async fn settle(step: Duration, rounds: u32) {
    let real_time = (step / 4).min(MAX_REAL_TIME_PER_ROUND);
    for _ in 0..rounds {
        tokio::time::advance(step).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        let _ = tokio::task::spawn_blocking(move || std::thread::sleep(real_time)).await;
    }
}

/// Drives the clock like [`settle`], but stops as soon as `condition` holds, and reports whether
/// it ever did.
///
/// Use this before asserting on anything a background task produces. A fixed round count is a
/// guess about how far that task gets per round; the guess holds on an idle laptop and does not on
/// a contended runner, which is what put five of this crate's tests -- and then a sixth -- on the
/// CI flake list while they passed locally every single time. Waiting for the condition itself
/// removes the guess rather than enlarging it.
///
/// Be generous with `max_rounds`. It bounds how long a genuine failure takes to report, and
/// nothing else: a passing run stops at the condition and never spends it.
pub(crate) async fn settle_until(
    step: Duration,
    max_rounds: u32,
    mut condition: impl FnMut() -> bool,
) -> bool {
    for _ in 0..max_rounds {
        if condition() {
            return true;
        }
        settle(step, 1).await;
    }
    condition()
}
