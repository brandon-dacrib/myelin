//! Process-wide metrics of this crate: what the room mirror (`crate::cluster::RoomMirror`) does
//! on a replica that reads rooms it does not own (decision 0022).
//!
//! Process-wide statics, like `hs_room::metrics`: the mirror runs far from any registry, and a
//! metric is only atomics. [`register_metrics`] puts them on `/metrics`; `hs-cli` calls it once
//! at startup.

use std::sync::LazyLock;
use std::sync::atomic::AtomicI64;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

/// The labels of `hs_user_mirror_full_reloads_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct ReloadLabels {
    reason: &'static str,
}

/// The labels of `hs_user_mirror_catchup_duration_seconds`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct DurationLabels {
    kind: &'static str,
}

/// `hs_user_mirror_rooms`: rooms this replica holds a read-only copy of.
static MIRROR_ROOMS: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_user_mirror_catchup_events_total`: timeline events copies absorbed incrementally.
static MIRROR_CATCHUP_EVENTS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_user_mirror_full_reloads_total{reason}`: whole-room loads of a copy, by why.
static MIRROR_FULL_RELOADS: LazyLock<Family<ReloadLabels, Counter>> =
    LazyLock::new(Family::default);

/// `hs_user_mirror_catchup_duration_seconds{kind}`: how long bringing a copy up to the store's
/// head took, `kind` `incremental` (a catch-up) or `full` (a whole load).
static MIRROR_CATCHUP_DURATION: LazyLock<Family<DurationLabels, Histogram>> = LazyLock::new(|| {
    Family::new_with_constructor(|| {
        Histogram::new([
            0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            2.5, 5.0,
        ])
    })
});

/// `hs_user_mirror_wakes_covered_total`: wakes for a mirrored room whose position the copy
/// already held, so nothing was read.
static MIRROR_WAKES_COVERED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Records how many rooms the mirror holds.
pub(crate) fn set_mirror_rooms(rooms: usize) {
    MIRROR_ROOMS.set(i64::try_from(rooms).unwrap_or(i64::MAX));
}

/// Records one incremental catch-up that absorbed `events` events in `elapsed`.
pub(crate) fn observe_catch_up(events: usize, elapsed: Duration) {
    MIRROR_CATCHUP_EVENTS.inc_by(events as u64);
    MIRROR_CATCHUP_DURATION
        .get_or_create(&DurationLabels {
            kind: "incremental",
        })
        .observe(elapsed.as_secs_f64());
}

/// Records one whole-room load of a copy, for `reason`, that took `elapsed`.
pub(crate) fn observe_full_reload(reason: &'static str, elapsed: Duration) {
    MIRROR_FULL_RELOADS
        .get_or_create(&ReloadLabels { reason })
        .inc();
    MIRROR_CATCHUP_DURATION
        .get_or_create(&DurationLabels { kind: "full" })
        .observe(elapsed.as_secs_f64());
}

/// Counts one wake the mirror had already covered.
pub(crate) fn count_wake_covered() {
    MIRROR_WAKES_COVERED.inc();
}

/// Registers this module's metrics into `registry`: `hs_user_mirror_rooms`,
/// `hs_user_mirror_catchup_events_total`, `hs_user_mirror_full_reloads_total{reason}`,
/// `hs_user_mirror_catchup_duration_seconds{kind}` and `hs_user_mirror_wakes_covered_total`
/// (decision 0022).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_user_mirror_rooms",
        "Rooms this replica holds a read-only copy of, to answer /sync for rooms another \
         replica owns",
        MIRROR_ROOMS.clone(),
    );
    // Counters are registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_user_mirror_catchup_events",
        "Timeline events room copies absorbed incrementally from the store",
        MIRROR_CATCHUP_EVENTS.clone(),
    );
    registry.register(
        "hs_user_mirror_full_reloads",
        "Whole-room loads of a room copy, by reason (first_read, rewritten, position_gap, \
         explicit_state, unexpected_row, regressed, error, incremental_off)",
        MIRROR_FULL_RELOADS.clone(),
    );
    registry.register(
        "hs_user_mirror_catchup_duration_seconds",
        "Time taken to bring a room copy up to the store's head, by kind (incremental, full)",
        MIRROR_CATCHUP_DURATION.clone(),
    );
    registry.register(
        "hs_user_mirror_wakes_covered",
        "Wakes for a mirrored room whose position the copy already held",
        MIRROR_WAKES_COVERED.clone(),
    );
}
