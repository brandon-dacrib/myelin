//! Process-wide metrics of this crate: what opening and feeding the production state store
//! costs (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`).
//!
//! Process-wide statics, like `hs_room::metrics` and `hs_kv::metrics`: a store is opened by a
//! room actor far from any registry, and a metric is only atomics. [`register_metrics`] puts
//! them on `/metrics`; `hs-cli` calls it once at startup.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

/// `hs_state_open_seconds`: how long `ProductionStateStore::open` took -- opening the durable
/// keyspaces and nothing else, since 2026-10-10. Before, the open was free and the room actor
/// paid for the store with a replay of the room's history.
static OPEN_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
        5.0,
    ])
});

/// `hs_state_events_replayed_total`: events ingested whose durable `state_at` row already
/// existed -- a replay of what the store already held. Zero once every room's actor loads
/// from the durable records instead of feeding the store its history again.
static EVENTS_REPLAYED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_state_resolution_events_cached`: event records (`ResolutionEvent`s) resident in the
/// stores' caches across every open room, read back from the store for state resolution and
/// kept until evicted.
static RESOLUTION_EVENTS_CACHED: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_state_migrations_total`: rooms whose durable records were built from their events on
/// first open after the change (`KvStateStore::mark_migrated`).
static MIGRATIONS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_state_event_records_read_total`: event records read from the store (for state
/// resolution, or an explicit-state ingestion). Zero on an open that reads only current state.
static EVENT_RECORDS_READ: LazyLock<Counter> = LazyLock::new(Counter::default);

/// A plain copy of the replayed count, for tests that run without a registry.
static EVENTS_REPLAYED_PLAIN: AtomicU64 = AtomicU64::new(0);

/// Records how long one `open` took.
pub(crate) fn observe_open(elapsed: Duration) {
    OPEN_SECONDS.observe(elapsed.as_secs_f64());
}

/// Counts `n` replayed events.
pub(crate) fn count_replayed(n: u64) {
    if n > 0 {
        EVENTS_REPLAYED.inc_by(n);
        EVENTS_REPLAYED_PLAIN.fetch_add(n, Ordering::Relaxed);
    }
}

/// `hs_state_events_replayed_total` as it stands.
#[must_use]
pub fn events_replayed() -> u64 {
    EVENTS_REPLAYED_PLAIN.load(Ordering::Relaxed)
}

/// Adjusts the resident event-record gauge by `delta`.
pub(crate) fn adjust_resolution_events_cached(delta: i64) {
    if delta > 0 {
        RESOLUTION_EVENTS_CACHED.inc_by(delta);
    } else if delta < 0 {
        RESOLUTION_EVENTS_CACHED.dec_by(-delta);
    }
}

/// `hs_state_resolution_events_cached` as it stands.
#[must_use]
pub fn resolution_events_cached() -> i64 {
    RESOLUTION_EVENTS_CACHED.get()
}

/// Counts one migrated room.
pub(crate) fn count_migration() {
    MIGRATIONS.inc();
}

/// `hs_state_migrations_total` as it stands.
#[must_use]
pub fn migrations() -> u64 {
    MIGRATIONS.get()
}

/// Counts `n` event records read from the store.
pub(crate) fn count_records_read(n: u64) {
    if n > 0 {
        EVENT_RECORDS_READ.inc_by(n);
    }
}

/// `hs_state_event_records_read_total` as it stands.
#[must_use]
pub fn event_records_read() -> u64 {
    EVENT_RECORDS_READ.get()
}

/// Registers this crate's metrics into `registry`: `hs_state_open_seconds`,
/// `hs_state_events_replayed_total`, `hs_state_resolution_events_cached`,
/// `hs_state_migrations_total` and `hs_state_event_records_read_total`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_state_open_seconds",
        "Time to open a room's production state store (its durable keyspaces)",
        OPEN_SECONDS.clone(),
    );
    // Counters are registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_state_events_replayed",
        "Events ingested into a state store that already held their durable state row",
        EVENTS_REPLAYED.clone(),
    );
    registry.register(
        "hs_state_resolution_events_cached",
        "Event records resident in the state stores' caches, across every open room",
        RESOLUTION_EVENTS_CACHED.clone(),
    );
    registry.register(
        "hs_state_migrations",
        "Rooms whose durable state records were built from their events on first open",
        MIGRATIONS.clone(),
    );
    registry.register(
        "hs_state_event_records_read",
        "Event records read from the store by state resolution or explicit-state ingestion",
        EVENT_RECORDS_READ.clone(),
    );
}
