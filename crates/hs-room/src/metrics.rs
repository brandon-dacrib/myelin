//! Process-wide metrics of this crate that are not about moderation (`crate::moderation` has
//! its own): how many attempts placing a new room's ID took, and the room-event search index.
//!
//! Process-wide statics, like `crate::moderation`'s: the code that observes them runs inside a
//! room actor's blocking construction or a background task with no registry at hand, and a
//! metric is only atomics. [`register_metrics`] puts them on `/metrics`; `hs-cli` calls it once
//! at startup.

use std::sync::LazyLock;
use std::sync::atomic::AtomicI64;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

/// `hs_room_create_room_id_attempts`: how many room IDs `RoomActor::create_placed` built before
/// one hashed to a room shard this replica owns (1 in single-node mode, about the number of
/// replicas in a cluster).
static CREATE_ROOM_ID_ATTEMPTS: LazyLock<Histogram> =
    LazyLock::new(|| Histogram::new([1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 256.0, 1024.0, 4096.0]));

/// `hs_room_search_indexed_events_total`: events whose words this replica wrote to the index.
static SEARCH_INDEXED_EVENTS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_room_search_index_documents`: events the index holds, as of this replica's last write.
static SEARCH_INDEX_DOCUMENTS: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_room_search_rooms_behind`: rooms the indexer's current catch-up still has to read.
static SEARCH_ROOMS_BEHIND: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_room_search_index_delay_seconds`: from an event's `origin_server_ts` to its words being
/// in the index -- the indexing lag.
static SEARCH_INDEX_DELAY: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 3600.0,
    ])
});

/// `hs_room_search_duration_seconds`: how long a `POST /search` took to answer.
static SEARCH_DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ])
});

/// Milliseconds since the Unix epoch, now.
pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Observes one room creation's ID attempts in `hs_room_create_room_id_attempts`.
pub(crate) fn observe_create_room_id_attempts(attempts: u32) {
    CREATE_ROOM_ID_ATTEMPTS.observe(f64::from(attempts));
}

/// Counts `added` newly indexed events and records that the index holds `documents`.
pub(crate) fn search_indexed(added: u64, documents: i64) {
    SEARCH_INDEXED_EVENTS.inc_by(added);
    SEARCH_INDEX_DOCUMENTS.set(documents);
}

/// Records how many rooms the indexer's catch-up has left.
pub(crate) fn set_search_rooms_behind(rooms: usize) {
    SEARCH_ROOMS_BEHIND.set(i64::try_from(rooms).unwrap_or(i64::MAX));
}

/// Observes one event's indexing lag, `delay_ms` milliseconds (negative, for a sender's clock
/// ahead of ours, counts as none).
pub(crate) fn observe_search_index_delay(delay_ms: i64) {
    SEARCH_INDEX_DELAY.observe(delay_ms.max(0) as f64 / 1000.0);
}

/// Observes one search's duration.
pub(crate) fn observe_search_duration(elapsed: std::time::Duration) {
    SEARCH_DURATION.observe(elapsed.as_secs_f64());
}

/// Registers this module's metrics into `registry`: `hs_room_create_room_id_attempts` (decision
/// 0020), and the search index's `hs_room_search_indexed_events_total`,
/// `hs_room_search_index_documents`, `hs_room_search_rooms_behind`,
/// `hs_room_search_index_delay_seconds` and `hs_room_search_duration_seconds` (decision 0021).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_room_create_room_id_attempts",
        "Room IDs built per room creation before one hashed to a room shard this replica owns",
        CREATE_ROOM_ID_ATTEMPTS.clone(),
    );
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_search_indexed_events",
        "Events whose words this replica wrote to the room-event search index",
        SEARCH_INDEXED_EVENTS.clone(),
    );
    registry.register(
        "hs_room_search_index_documents",
        "Events the room-event search index holds, as of this replica's last write to it",
        SEARCH_INDEX_DOCUMENTS.clone(),
    );
    registry.register(
        "hs_room_search_rooms_behind",
        "Rooms the search indexer's current catch-up still has to read (0 when caught up)",
        SEARCH_ROOMS_BEHIND.clone(),
    );
    registry.register(
        "hs_room_search_index_delay_seconds",
        "Time from an event's origin_server_ts to its words being in the search index",
        SEARCH_INDEX_DELAY.clone(),
    );
    registry.register(
        "hs_room_search_duration_seconds",
        "Time taken to answer POST /search",
        SEARCH_DURATION.clone(),
    );
}
