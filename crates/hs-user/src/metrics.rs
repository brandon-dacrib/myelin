//! Process-wide metrics of this crate: what the room mirror (`crate::cluster::RoomMirror`) does
//! on a replica that reads rooms it does not own (decision 0022), what a room update's fan-out
//! costs its owner's session hub (`crate::hub::SessionHub`), what retention prunes, and whether
//! the hub keeps up with the rooms (`crate::hub_progress`).
//!
//! Process-wide statics, like `hs_room::metrics`: the mirror runs far from any registry, and a
//! metric is only atomics. [`register_metrics`] puts them on `/metrics`; `hs-cli` calls it once
//! at startup.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, AtomicU64};
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
            2.5, 5.0, 10.0, 30.0, 60.0,
        ])
    })
});

/// `hs_user_mirror_wakes_covered_total`: wakes for a mirrored room whose position the copy
/// already held, so nothing was read.
static MIRROR_WAKES_COVERED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// The labels of `hs_user_fan_out_transactions_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

/// The labels of `hs_user_pruned_entries_total` and `hs_user_compactions_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct StreamLabels {
    stream: &'static str,
}

/// `hs_user_fan_out_duration_seconds`: how long writing one room update's membership records
/// and feed entries for every member took, on the room's owner.
static FAN_OUT_DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
        60.0,
    ])
});

/// `hs_user_fan_out_members_total`: members a room update was fanned out to.
static FAN_OUT_MEMBERS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_user_fan_out_transactions_total{outcome}`: store transactions fan-outs took, `batched`
/// (many members in one) or `fallback` (one member on their own after a batch kept conflicting).
static FAN_OUT_TRANSACTIONS: LazyLock<Family<OutcomeLabels, Counter>> =
    LazyLock::new(Family::default);

/// `hs_user_pruned_entries_total{stream}`: entries retention deleted, from `feed` (users'
/// feeds) or `hot_room_stream`.
static PRUNED_ENTRIES: LazyLock<Family<StreamLabels, Counter>> = LazyLock::new(Family::default);

/// `hs_user_compactions_total{stream}`: compaction runs, by stream.
static COMPACTIONS: LazyLock<Family<StreamLabels, Counter>> = LazyLock::new(Family::default);

/// `hs_user_hub_published_seq`: the newest update number the room registry has published.
static HUB_PUBLISHED: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_user_hub_consumed_seq`: the session hub's consumed mark.
static HUB_CONSUMED: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_user_hub_lag_updates`: published minus consumed.
static HUB_LAG: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_user_hub_oldest_pending_seconds`: how long the oldest update the hub has received and not
/// finished has been waiting; `0` when there is none.
static HUB_OLDEST_PENDING: LazyLock<Gauge<f64, AtomicU64>> = LazyLock::new(Gauge::default);

/// `hs_user_hub_stuck_updates`: updates in hand whose step has not moved for the stuck-step
/// threshold.
static HUB_STUCK: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_user_hub_stuck_warnings_total`: warnings the hub's watchdog logged about an update stuck
/// at one step.
static HUB_STUCK_WARNINGS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Records the hub's position on the room stream: `published` by the registry, `consumed` by
/// the hub.
pub(crate) fn set_hub_position(published: u64, consumed: u64) {
    HUB_PUBLISHED.set(i64::try_from(published).unwrap_or(i64::MAX));
    HUB_CONSUMED.set(i64::try_from(consumed).unwrap_or(i64::MAX));
    HUB_LAG.set(i64::try_from(published.saturating_sub(consumed)).unwrap_or(i64::MAX));
}

/// Records the watchdog's view: how long the oldest pending update has waited, and how many
/// updates in hand are stuck.
pub(crate) fn set_hub_health(oldest_pending: Duration, stuck: usize) {
    HUB_OLDEST_PENDING.set(oldest_pending.as_secs_f64());
    HUB_STUCK.set(i64::try_from(stuck).unwrap_or(i64::MAX));
}

/// Counts one stuck-update warning.
pub(crate) fn count_stuck_warning() {
    HUB_STUCK_WARNINGS.inc();
}

/// Records one room update's fan-out to `members` members in `elapsed`, taking `transactions`
/// store transactions of which `fallbacks` were single members.
pub(crate) fn observe_fan_out(
    members: usize,
    elapsed: Duration,
    transactions: usize,
    fallbacks: usize,
) {
    FAN_OUT_DURATION.observe(elapsed.as_secs_f64());
    FAN_OUT_MEMBERS.inc_by(members as u64);
    FAN_OUT_TRANSACTIONS
        .get_or_create(&OutcomeLabels { outcome: "batched" })
        .inc_by(transactions.saturating_sub(fallbacks) as u64);
    if fallbacks > 0 {
        FAN_OUT_TRANSACTIONS
            .get_or_create(&OutcomeLabels {
                outcome: "fallback",
            })
            .inc_by(fallbacks as u64);
    }
}

/// Records one compaction of `stream` that deleted `pruned` entries.
pub(crate) fn observe_compaction(stream: &'static str, pruned: usize) {
    COMPACTIONS.get_or_create(&StreamLabels { stream }).inc();
    PRUNED_ENTRIES
        .get_or_create(&StreamLabels { stream })
        .inc_by(pruned as u64);
}

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
/// (decision 0022); `hs_user_fan_out_duration_seconds`, `hs_user_fan_out_members_total` and
/// `hs_user_fan_out_transactions_total{outcome}` (the owner's fan-out);
/// `hs_user_pruned_entries_total{stream}` and `hs_user_compactions_total{stream}` (retention);
/// `hs_user_hub_published_seq`, `hs_user_hub_consumed_seq`, `hs_user_hub_lag_updates`,
/// `hs_user_hub_oldest_pending_seconds`, `hs_user_hub_stuck_updates` and
/// `hs_user_hub_stuck_warnings_total` (whether the session hub keeps up with the rooms).
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
    registry.register(
        "hs_user_fan_out_duration_seconds",
        "Time the room owner's session hub took to write one room update's membership records \
         and feed entries for every member",
        FAN_OUT_DURATION.clone(),
    );
    registry.register(
        "hs_user_fan_out_members",
        "Members room updates were fanned out to",
        FAN_OUT_MEMBERS.clone(),
    );
    registry.register(
        "hs_user_fan_out_transactions",
        "Store transactions fan-outs took, by outcome (batched, fallback)",
        FAN_OUT_TRANSACTIONS.clone(),
    );
    registry.register(
        "hs_user_pruned_entries",
        "Entries retention deleted, by stream (feed, hot_room_stream)",
        PRUNED_ENTRIES.clone(),
    );
    registry.register(
        "hs_user_compactions",
        "Retention compaction runs, by stream (feed, hot_room_stream)",
        COMPACTIONS.clone(),
    );
    registry.register(
        "hs_user_hub_published_seq",
        "The newest room update number the room registry has published",
        HUB_PUBLISHED.clone(),
    );
    registry.register(
        "hs_user_hub_consumed_seq",
        "The session hub's consumed mark: every room update numbered up to it is in the feeds",
        HUB_CONSUMED.clone(),
    );
    registry.register(
        "hs_user_hub_lag_updates",
        "Room updates published and not yet in the feeds (published minus consumed)",
        HUB_LAG.clone(),
    );
    registry.register(
        "hs_user_hub_oldest_pending_seconds",
        "How long the oldest room update the session hub has received and not finished has \
         been waiting; 0 when none",
        HUB_OLDEST_PENDING.clone(),
    );
    registry.register(
        "hs_user_hub_stuck_updates",
        "Room updates the session hub has had at one step for longer than 30 seconds",
        HUB_STUCK.clone(),
    );
    registry.register(
        "hs_user_hub_stuck_warnings",
        "Warnings the session hub's watchdog logged about a room update stuck at one step",
        HUB_STUCK_WARNINGS.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hub's gauges are on `/metrics`, with their values.
    #[test]
    fn the_hub_gauges_are_rendered() {
        let mut registry = prometheus_client::registry::Registry::default();
        register_metrics(&mut registry);
        set_hub_position(10, 7);
        set_hub_health(Duration::from_millis(1500), 1);
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        for line in [
            "hs_user_hub_published_seq 10",
            "hs_user_hub_consumed_seq 7",
            "hs_user_hub_lag_updates 3",
            "hs_user_hub_oldest_pending_seconds 1.5",
            "hs_user_hub_stuck_updates 1",
            "hs_user_hub_stuck_warnings_total",
        ] {
            assert!(text.contains(line), "{line} in:\n{text}");
        }
    }
}
