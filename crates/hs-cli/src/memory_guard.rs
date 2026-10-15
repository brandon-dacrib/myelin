//! The memory guard: a process-level safety net that stops taking other servers' requests while
//! this process is over its memory budget, so that what it holds can drain before the kernel
//! kills it -- or, on a node shared with many pods, before the node itself runs out.
//!
//! Every [`CHECK_INTERVAL`] the guard reads the process's resident memory
//! (`crate::process_metrics::process_memory`) and the container's limit
//! ([`cgroup_memory_limit`]); once the one is over `federation.inbound_memory_shed_fraction` of
//! the other it turns on `hs_federation::transport::set_inbound_shedding`, and every inbound
//! federation request is answered `429` with a `Retry-After` until resident memory is back under
//! the line less [`HYSTERESIS`]. Each change is logged (`warn` on, `info` off), and
//! `hs_process_memory_limit_bytes`, `hs_process_memory_budget_ratio`,
//! `hs_federation_inbound_shedding` and `hs_federation_inbound_shed_total` show it on
//! `/metrics`. A process with no limit (no cgroup, or none set) never sheds.
//!
//! Why: on 2026-10-15 the demo, with federation on, grew some 120 MB a minute for twenty
//! minutes (abandoned room loads, `hs_room::registry::RoomRegistry::get_or_load`) until its node
//! ran out of memory and went `NotReady`; the pod's 3 GiB limit was never reached because the
//! node had less to give. Nothing in the process looked at its own size. The bug that grew it is
//! fixed; this is the bound for the next one (`docs/status/06-federation.md`, "2026-10-15: the
//! memory climb").

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How often the guard compares the process's memory with its limit.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// How far under the line resident memory must fall before shedding stops: five points of the
/// limit, so that a process hovering at the line does not flap.
pub const HYSTERESIS: f64 = 0.05;

/// Above this a cgroup v1 `memory.limit_in_bytes` means "no limit" (the kernel reports
/// `PAGE_COUNTER_MAX` pages, about 2^63 bytes, rounded to the page size).
const UNLIMITED_V1: u64 = 1 << 60;

/// The memory limit of this process's container, in bytes: cgroup v2's `memory.max`, else
/// cgroup v1's `memory.limit_in_bytes`. `None` without a cgroup, or with no limit set.
#[must_use]
pub fn cgroup_memory_limit() -> Option<u64> {
    read_limit("/sys/fs/cgroup/memory.max")
        .or_else(|| read_limit("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
}

fn read_limit(path: &str) -> Option<u64> {
    parse_cgroup_limit(&std::fs::read_to_string(path).ok()?)
}

/// A cgroup memory limit file's contents as bytes: `None` for `max` (v2's "no limit"), for a
/// v1 limit so large it means the same, for zero, or for anything that is not a number.
#[must_use]
pub fn parse_cgroup_limit(text: &str) -> Option<u64> {
    let text = text.trim();
    if text == "max" {
        return None;
    }
    text.parse::<u64>()
        .ok()
        .filter(|bytes| *bytes > 0 && *bytes < UNLIMITED_V1)
}

/// The process's resident memory as a share of its limit, with both: `None` where either cannot
/// be read.
#[must_use]
pub fn memory_budget() -> Option<MemoryBudget> {
    let limit_bytes = cgroup_memory_limit()?;
    let resident_bytes = crate::process_metrics::process_memory()?.resident_bytes;
    Some(MemoryBudget {
        resident_bytes,
        limit_bytes,
    })
}

/// The process's resident memory and its container's limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    /// Resident memory, in bytes.
    pub resident_bytes: u64,
    /// The container's memory limit, in bytes.
    pub limit_bytes: u64,
}

impl MemoryBudget {
    /// Resident memory as a share of the limit (`1.0` is the limit).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn ratio(&self) -> f64 {
        self.resident_bytes as f64 / self.limit_bytes.max(1) as f64
    }
}

/// Whether inbound federation should be shed, given whether it is now, the share of the limit
/// in use, and the configured line (`0` or less: never). On at the line; off only once under
/// it less [`HYSTERESIS`].
#[must_use]
pub fn should_shed(shedding: bool, ratio: f64, fraction: f64) -> bool {
    if fraction <= 0.0 {
        return false;
    }
    if shedding {
        ratio >= fraction - HYSTERESIS
    } else {
        ratio >= fraction
    }
}

/// The running guard: a task checking every [`CHECK_INTERVAL`], stopped when this is dropped.
pub struct MemoryGuard {
    /// `federation.inbound_memory_shed_fraction`, as `f64` bits, read at each check.
    fraction: Arc<AtomicU64>,
    task: tokio::task::AbortHandle,
}

impl MemoryGuard {
    /// Starts the guard on the current runtime with `fraction` as the line. Must be called from
    /// within a Tokio runtime.
    #[must_use]
    pub fn start(fraction: f64) -> Self {
        let fraction = Arc::new(AtomicU64::new(fraction.to_bits()));
        let read = fraction.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(CHECK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                check(f64::from_bits(read.load(Ordering::Relaxed)));
            }
        })
        .abort_handle();
        Self { fraction, task }
    }

    /// Changes the line; applies at the next check. Logged when it changes.
    pub fn set_fraction(&self, fraction: f64) {
        let previous = f64::from_bits(self.fraction.swap(fraction.to_bits(), Ordering::Relaxed));
        if previous.to_bits() != fraction.to_bits() {
            tracing::info!(
                inbound_memory_shed_fraction = fraction,
                "the memory budget above which other servers' requests are refused is now in \
                 force (0 turns it off)"
            );
        }
    }
}

impl Drop for MemoryGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One check: reads the budget and turns shedding on or off, logging the change.
fn check(fraction: f64) {
    let shedding = hs_federation::transport::inbound_shedding();
    let Some(budget) = memory_budget() else {
        if shedding {
            hs_federation::transport::set_inbound_shedding(false);
            tracing::info!(
                "the memory limit can no longer be read; taking other servers' requests again"
            );
        }
        return;
    };
    let ratio = budget.ratio();
    let shed = should_shed(shedding, ratio, fraction);
    if shed == shedding {
        return;
    }
    hs_federation::transport::set_inbound_shedding(shed);
    if shed {
        tracing::warn!(
            resident_bytes = budget.resident_bytes,
            limit_bytes = budget.limit_bytes,
            ratio,
            inbound_memory_shed_fraction = fraction,
            "the process is over its memory budget: refusing other servers' requests (429, \
             retried later) until it is back under it (hs_process_memory_budget_ratio, \
             hs_federation_inbound_shedding)"
        );
    } else {
        tracing::info!(
            resident_bytes = budget.resident_bytes,
            limit_bytes = budget.limit_bytes,
            ratio,
            inbound_memory_shed_fraction = fraction,
            "the process is back under its memory budget: taking other servers' requests again"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cgroup_limit_is_read_and_no_limit_is_none() {
        assert_eq!(parse_cgroup_limit("3221225472\n"), Some(3_221_225_472));
        assert_eq!(parse_cgroup_limit("max\n"), None);
        // cgroup v1 with no limit: PAGE_COUNTER_MAX pages.
        assert_eq!(parse_cgroup_limit("9223372036854771712"), None);
        assert_eq!(parse_cgroup_limit("0"), None);
        assert_eq!(parse_cgroup_limit("garbage"), None);
    }

    #[test]
    fn shedding_starts_at_the_line_and_stops_below_it_less_the_hysteresis() {
        assert!(!should_shed(false, 0.84, 0.85));
        assert!(should_shed(false, 0.85, 0.85));
        assert!(
            should_shed(true, 0.81, 0.85),
            "stays on just under the line"
        );
        assert!(
            !should_shed(true, 0.79, 0.85),
            "off once five points under it"
        );
        assert!(!should_shed(false, 2.0, 0.0), "0 turns it off");
        assert!(!should_shed(true, 2.0, 0.0));
    }

    #[test]
    fn the_ratio_is_resident_over_limit() {
        let budget = MemoryBudget {
            resident_bytes: 1_500,
            limit_bytes: 3_000,
        };
        assert!((budget.ratio() - 0.5).abs() < f64::EPSILON);
    }
}
