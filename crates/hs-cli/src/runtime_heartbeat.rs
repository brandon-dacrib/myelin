//! A heartbeat on the server's async runtime, and a watchdog thread that says when it stops.
//!
//! Every request, `/health/live` included, is answered on the runtime's worker threads. When
//! every one of them is busy or blocked -- on 2026-10-14, parked in the store's commit lock
//! behind the outbound federation sender's bookkeeping (`docs/status/06-federation.md`, "The
//! boot freeze") -- nothing is answered, the liveness probe fails and the pod is killed, and
//! until now nothing in the logs or on `/metrics` said so: a frozen runtime cannot log that it
//! is frozen, and cannot be scraped either.
//!
//! - **The heartbeat** is a task on the runtime that sleeps [`HEARTBEAT_INTERVAL`] and records
//!   how late it woke: `hs_runtime_heartbeat_lateness_seconds`, a histogram of every beat, and
//!   `hs_runtime_heartbeat_lateness_max_seconds`, the latest beat's lateness or the worst of the
//!   last minute, whichever is greater. A beat later than [`LATE_BEAT`] is logged at `warn`
//!   (at most once per [`LATE_BEAT_WARNING_INTERVAL`]).
//! - **The watchdog** is an operating-system thread, outside the runtime, that looks at the
//!   heartbeat every [`WATCHDOG_INTERVAL`]. When the heartbeat has not beaten for
//!   [`STALL_WARNING`] it logs at `warn` that the runtime is stalled -- written while the stall
//!   lasts, from a thread the stall cannot hold -- and again, at `info`, when the runtime runs
//!   again, with how long the stall was.
//!
//! Neither changes what `/health/live` answers: a runtime that cannot answer it still fails
//! the probe, as it should. This only says why, while it happens.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// How often the heartbeat beats.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);

/// A beat this late is logged at `warn`.
pub const LATE_BEAT: Duration = Duration::from_secs(1);

/// At most one warning about late beats this often.
pub const LATE_BEAT_WARNING_INTERVAL: Duration = Duration::from_secs(10);

/// How often the watchdog thread looks at the heartbeat.
pub const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

/// The watchdog warns once the heartbeat has not beaten for this long: under the liveness
/// probe's own timeout (five seconds on the demo), so the warning is in the log before the
/// probe fails.
pub const STALL_WARNING: Duration = Duration::from_secs(3);

/// How long the worst-of-the-window gauge remembers a late beat.
const MAX_WINDOW: Duration = Duration::from_secs(60);

static LATENESS: std::sync::LazyLock<Histogram> = std::sync::LazyLock::new(|| {
    Histogram::new([
        0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
    ])
});
static LATENESS_MAX: std::sync::LazyLock<Gauge<f64, AtomicU64>> =
    std::sync::LazyLock::new(Gauge::default);

/// Registers `hs_runtime_heartbeat_lateness_seconds` and
/// `hs_runtime_heartbeat_lateness_max_seconds` (see the module docs).
pub fn register_metrics(registry: &mut Registry) {
    registry.register(
        "hs_runtime_heartbeat_lateness_seconds",
        "How late the async runtime's heartbeat woke, per beat (every 250 ms): how long every \
         worker thread was busy or blocked before a timer could run",
        LATENESS.clone(),
    );
    registry.register(
        "hs_runtime_heartbeat_lateness_max_seconds",
        "The worst lateness of the runtime's heartbeat over the last minute",
        LATENESS_MAX.clone(),
    );
}

/// The worst lateness the last minute's beats recorded, as `hs_runtime_heartbeat_lateness_max_seconds`
/// has it (for tests).
#[must_use]
pub fn lateness_max_seconds() -> f64 {
    LATENESS_MAX.get()
}

/// What the heartbeat task and the watchdog thread share.
struct Pulse {
    /// When the heartbeat last beat, in microseconds since `origin`.
    last_beat_us: AtomicU64,
    origin: Instant,
    stop: AtomicBool,
}

impl Pulse {
    fn now_us(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// The running heartbeat and watchdog. Dropping it stops both.
pub struct RuntimeHeartbeat {
    pulse: Arc<Pulse>,
    task: tokio::task::AbortHandle,
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl RuntimeHeartbeat {
    /// Starts the heartbeat on the current runtime and the watchdog thread beside it. Must be
    /// called from within a Tokio runtime.
    #[must_use]
    pub fn start() -> Self {
        Self::start_with(HEARTBEAT_INTERVAL, STALL_WARNING, WATCHDOG_INTERVAL)
    }

    fn start_with(interval: Duration, stall_after: Duration, watch_every: Duration) -> Self {
        let pulse = Arc::new(Pulse {
            last_beat_us: AtomicU64::new(0),
            origin: Instant::now(),
            stop: AtomicBool::new(false),
        });
        let beating = pulse.clone();
        let task = tokio::spawn(async move {
            let mut window_start = Instant::now();
            let (mut window_max, mut previous_max) = (0.0_f64, 0.0_f64);
            let mut last_warning: Option<Instant> = None;
            loop {
                let asleep = Instant::now();
                tokio::time::sleep(interval).await;
                let lateness = asleep.elapsed().saturating_sub(interval);
                beating
                    .last_beat_us
                    .store(beating.now_us(), Ordering::Release);
                let seconds = lateness.as_secs_f64();
                LATENESS.observe(seconds);
                if window_start.elapsed() >= MAX_WINDOW {
                    previous_max = window_max;
                    window_max = 0.0;
                    window_start = Instant::now();
                }
                window_max = window_max.max(seconds);
                LATENESS_MAX.set(window_max.max(previous_max));
                if lateness >= LATE_BEAT
                    && last_warning.is_none_or(|at| at.elapsed() >= LATE_BEAT_WARNING_INTERVAL)
                {
                    last_warning = Some(Instant::now());
                    tracing::warn!(
                        late_ms = u64::try_from(lateness.as_millis()).unwrap_or(u64::MAX),
                        "the async runtime ran its heartbeat late: every worker thread was busy \
                         or blocked for that long, and no request was answered meanwhile"
                    );
                }
            }
        })
        .abort_handle();
        let watched = pulse.clone();
        let watchdog = std::thread::Builder::new()
            .name("hs-runtime-watchdog".to_owned())
            .spawn(move || watch(&watched, stall_after, watch_every))
            .map_err(|error| {
                tracing::warn!(%error, "could not start the runtime watchdog thread");
            })
            .ok();
        Self {
            pulse,
            task,
            watchdog,
        }
    }
}

/// The watchdog thread's loop: see the module docs.
fn watch(pulse: &Pulse, stall_after: Duration, every: Duration) {
    let stall_us = u64::try_from(stall_after.as_micros()).unwrap_or(u64::MAX);
    let mut stalled_since_us: Option<u64> = None;
    while !pulse.stop.load(Ordering::Acquire) {
        std::thread::sleep(every);
        let last = pulse.last_beat_us.load(Ordering::Acquire);
        let now = pulse.now_us();
        let silent_us = now.saturating_sub(last);
        match stalled_since_us {
            None if silent_us >= stall_us => {
                stalled_since_us = Some(last);
                tracing::warn!(
                    silent_ms = silent_us / 1000,
                    "the async runtime has not run its heartbeat for over three seconds: every \
                     worker thread is busy or blocked, and no request -- /health/live included \
                     -- is being answered"
                );
            }
            Some(since) if last > since => {
                stalled_since_us = None;
                tracing::info!(
                    stalled_ms = last.saturating_sub(since) / 1000,
                    "the async runtime is running again after a stall"
                );
            }
            _ => {}
        }
    }
}

impl Drop for RuntimeHeartbeat {
    fn drop(&mut self) {
        self.pulse.stop.store(true, Ordering::Release);
        self.task.abort();
        // Not joined: this may be dropped on a runtime thread, and the watchdog notices the
        // stop at its next look, within `WATCHDOG_INTERVAL`, and ends on its own.
        drop(self.watchdog.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime whose only worker is blocked is noticed by the watchdog while it lasts, and
    /// the late beat is measured once it ends.
    #[test]
    fn a_blocked_runtime_is_noticed_while_it_lasts_and_measured_after() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let heartbeat = runtime.block_on(async {
            RuntimeHeartbeat::start_with(
                Duration::from_millis(20),
                Duration::from_millis(200),
                Duration::from_millis(20),
            )
        });
        // Let it beat, then block the one worker thread for longer than the stall threshold.
        std::thread::sleep(Duration::from_millis(100));
        let before = heartbeat.pulse.last_beat_us.load(Ordering::Acquire);
        assert!(before > 0, "the heartbeat beat");
        runtime.block_on(async {
            tokio::spawn(async {
                // Deliberately blocking the worker: this is what the heartbeat is for.
                std::thread::sleep(Duration::from_millis(600));
            })
            .await
            .unwrap();
        });
        std::thread::sleep(Duration::from_millis(100));
        let after = heartbeat.pulse.last_beat_us.load(Ordering::Acquire);
        assert!(after > before, "the heartbeat beat again after the block");
        assert!(
            lateness_max_seconds() >= 0.4,
            "the late beat was measured: {}",
            lateness_max_seconds()
        );
        drop(heartbeat);
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    /// Registers on the shared metrics the way `hs serve` does.
    #[test]
    fn registers_on_the_shared_metrics() {
        let metrics = hs_telemetry::metrics::Metrics::new();
        metrics.with_registry(register_metrics);
        let text = metrics.encode_to_string().unwrap();
        assert!(
            text.contains("hs_runtime_heartbeat_lateness_seconds_bucket"),
            "{text}"
        );
        assert!(
            text.contains("hs_runtime_heartbeat_lateness_max_seconds"),
            "{text}"
        );
    }
}
