//! Process-wide metrics of this crate that are not about moderation (`crate::moderation` has
//! its own): how many attempts placing a new room's ID took and how many IDs were found taken,
//! room upgrades, and the room-event search index.
//!
//! Process-wide statics, like `crate::moderation`'s: the code that observes them runs inside a
//! room actor's blocking construction or a background task with no registry at hand, and a
//! metric is only atomics. [`register_metrics`] puts them on `/metrics`; `hs-cli` calls it once
//! at startup.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

/// `hs_room_create_room_id_attempts`: how many room IDs `RoomActor::create_placed` built before
/// one hashed to a room shard this replica owns (1 in single-node mode, about the number of
/// replicas in a cluster).
static CREATE_ROOM_ID_ATTEMPTS: LazyLock<Histogram> =
    LazyLock::new(|| Histogram::new([1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 256.0, 1024.0, 4096.0]));

/// `hs_room_create_room_id_taken_total`: room IDs `RoomActor::create_placed` built for a new
/// room that a room already had -- a version-12 create identical to an earlier one (the same
/// creator and content in the same millisecond), or a minted opaque ID that was not new. Each
/// is one of the attempts `hs_room_create_room_id_attempts` counts, and was built again.
static CREATE_ROOM_ID_TAKEN: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_room_outlier_state_rows_repaired_total`: outliers placed in a timeline before placement
/// recorded the state at them (status 04 session 13), found on a room load with no
/// `state_snapshots` row and given one (`RoomActor::load`, "Placed outliers without a state
/// row"). Counts once per room load that found some, so a non-owner's copies count too.
static OUTLIER_STATE_ROWS_REPAIRED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_room_search_indexed_events_total`: events whose words this replica wrote to the index.
static SEARCH_INDEXED_EVENTS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_room_soft_failed_events_total`: events received over federation that passed the auth
/// rules at the state before them and failed them at the room's current state
/// (`crate::actor::soft_fail`): held, kept from clients.
static SOFT_FAILED_EVENTS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Counts one soft-failed event.
pub(crate) fn record_soft_failed_event() {
    SOFT_FAILED_EVENTS.inc();
}

/// `hs_room_soft_failed_events_total` as it stands.
#[must_use]
pub fn soft_failed_events() -> u64 {
    SOFT_FAILED_EVENTS.get()
}

/// `hs_room_search_index_documents`: events the index holds, as of this replica's last write.
static SEARCH_INDEX_DOCUMENTS: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_room_search_rooms_behind`: rooms the indexer's current catch-up still has to read.
static SEARCH_ROOMS_BEHIND: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// Whether `SEARCH_INDEX_DOCUMENTS` has been set since this process started; until then it reads
/// 0 without meaning it.
static SEARCH_INDEX_DOCUMENTS_SET: AtomicBool = AtomicBool::new(false);

/// Whether the indexer has counted the rooms it is behind on since this process started; until
/// its first catch-up does, `SEARCH_ROOMS_BEHIND` reads 0 without meaning it.
static SEARCH_ROOMS_BEHIND_SET: AtomicBool = AtomicBool::new(false);

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

/// `hs_room_upgrades_total{outcome}`: room upgrades this replica ran, by how they ended --
/// `completed`, or `replacement_orphaned` when one half was written and the other failed (a
/// tombstone naming a room whose create failed, or a replacement room whose tombstone the old
/// room refused).
static ROOM_UPGRADES: LazyLock<Family<Vec<(String, String)>, Counter>> =
    LazyLock::new(Family::default);

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

/// Counts one room upgrade in `hs_room_upgrades_total` with its `outcome`.
pub(crate) fn count_room_upgrade(outcome: &str) {
    ROOM_UPGRADES
        .get_or_create(&vec![("outcome".to_owned(), outcome.to_owned())])
        .inc();
}

/// Counts one new room's ID found already taken in `hs_room_create_room_id_taken_total`.
pub(crate) fn count_create_room_id_taken() {
    CREATE_ROOM_ID_TAKEN.inc();
}

/// Counts `repaired` placed outliers given a state row on a room load in
/// `hs_room_outlier_state_rows_repaired_total`.
pub(crate) fn count_outlier_state_rows_repaired(repaired: usize) {
    OUTLIER_STATE_ROWS_REPAIRED.inc_by(repaired as u64);
}

/// `hs_room_create_room_id_taken_total` as it stands, for tests.
#[cfg(test)]
pub(crate) fn create_room_id_taken() -> u64 {
    CREATE_ROOM_ID_TAKEN.get()
}

/// Counts `added` newly indexed events and records that the index holds `documents`.
pub(crate) fn search_indexed(added: u64, documents: i64) {
    SEARCH_INDEXED_EVENTS.inc_by(added);
    SEARCH_INDEX_DOCUMENTS.set(documents);
    SEARCH_INDEX_DOCUMENTS_SET.store(true, Ordering::Release);
}

/// Records how many rooms the indexer's catch-up has left.
pub(crate) fn set_search_rooms_behind(rooms: usize) {
    SEARCH_ROOMS_BEHIND.set(i64::try_from(rooms).unwrap_or(i64::MAX));
    SEARCH_ROOMS_BEHIND_SET.store(true, Ordering::Release);
}

/// How far this replica's room-event search index is behind, as `/metrics` serves it in
/// `hs_room_search_rooms_behind` and `hs_room_search_index_documents`: what `hs-cli` puts in the
/// admin API's `GET /cluster` (`ClusterStatus.search_rooms_behind` and
/// `.search_index_documents`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchIndexLag {
    /// Rooms this replica owns whose newest events are not yet in its search index: what the
    /// indexer's current catch-up still has to read. 0 when search is up to date. `None` until
    /// the indexer has counted once since the process started.
    pub rooms_behind: Option<u64>,
    /// Events the index holds, as of this replica's last write to it. `None` until the indexer
    /// has read or written the count once since the process started.
    pub documents: Option<u64>,
}

/// A gauge's value, or `None` when it has not been `set` (it reads 0 then without meaning it).
/// A negative value, which the indexer never sets, reads as 0.
fn gauge_value(value: i64, set: bool) -> Option<u64> {
    set.then(|| u64::try_from(value).unwrap_or(0))
}

/// This replica's search-index lag now (see [`SearchIndexLag`]).
#[must_use]
pub fn search_index_lag() -> SearchIndexLag {
    SearchIndexLag {
        rooms_behind: gauge_value(
            SEARCH_ROOMS_BEHIND.get(),
            SEARCH_ROOMS_BEHIND_SET.load(Ordering::Acquire),
        ),
        documents: gauge_value(
            SEARCH_INDEX_DOCUMENTS.get(),
            SEARCH_INDEX_DOCUMENTS_SET.load(Ordering::Acquire),
        ),
    }
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
/// 0020), `hs_room_create_room_id_taken_total`, `hs_room_upgrades_total`, and the search index's
/// `hs_room_search_indexed_events_total`, `hs_room_search_index_documents`,
/// `hs_room_search_rooms_behind`, `hs_room_search_index_delay_seconds` and
/// `hs_room_search_duration_seconds` (decision 0021).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_room_create_room_id_attempts",
        "Room IDs built per room creation before one hashed to a room shard this replica owns \
         and was not already a room's",
        CREATE_ROOM_ID_ATTEMPTS.clone(),
    );
    registry.register(
        "hs_room_upgrades",
        "Room upgrades this replica ran, by outcome (completed, replacement_orphaned)",
        ROOM_UPGRADES.clone(),
    );
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_create_room_id_taken",
        "Room IDs built for a new room that a room already had (the same creator and content \
         in the same millisecond), each built again",
        CREATE_ROOM_ID_TAKEN.clone(),
    );
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_outlier_state_rows_repaired",
        "Placed outliers found on a room load with no state row (placed before the state at \
         backfilled history was recorded) and given one",
        OUTLIER_STATE_ROWS_REPAIRED.clone(),
    );
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_soft_failed_events",
        "Events received over federation that the auth rules allow at the state before them \
         and refuse at the room's current state: held, kept from clients",
        SOFT_FAILED_EVENTS.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_gauge_is_unknown_and_a_set_one_is_its_value() {
        assert_eq!(gauge_value(0, false), None);
        assert_eq!(gauge_value(7, false), None);
        assert_eq!(gauge_value(0, true), Some(0));
        assert_eq!(gauge_value(7, true), Some(7));
        assert_eq!(gauge_value(-1, true), Some(0));
    }

    #[test]
    fn the_lag_is_known_once_the_indexer_has_recorded_it() {
        // Process-wide statics that other tests' indexers also set: only that the values become
        // known can be asserted here, not which numbers they hold.
        set_search_rooms_behind(3);
        search_indexed(0, 12);
        let lag = search_index_lag();
        assert!(lag.rooms_behind.is_some());
        assert!(lag.documents.is_some());
    }
}
