//! Process-wide metrics of this crate that are not about moderation (`crate::moderation` has
//! its own): how many attempts placing a new room's ID took.
//!
//! Process-wide statics, like `crate::moderation`'s: the code that observes them runs inside a
//! room actor's blocking construction with no registry at hand, and a metric is only atomics.
//! [`register_metrics`] puts them on `/metrics`; `hs-cli` calls it once at startup.

use std::sync::LazyLock;

use prometheus_client::metrics::histogram::Histogram;

/// `hs_room_create_room_id_attempts`: how many room IDs `RoomActor::create_placed` built before
/// one hashed to a room shard this replica owns (1 in single-node mode, about the number of
/// replicas in a cluster).
static CREATE_ROOM_ID_ATTEMPTS: LazyLock<Histogram> =
    LazyLock::new(|| Histogram::new([1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 256.0, 1024.0, 4096.0]));

/// Observes one room creation's ID attempts in `hs_room_create_room_id_attempts`.
pub(crate) fn observe_create_room_id_attempts(attempts: u32) {
    CREATE_ROOM_ID_ATTEMPTS.observe(f64::from(attempts));
}

/// Registers this module's metrics into `registry`: `hs_room_create_room_id_attempts`, the
/// histogram of how many room IDs a room creation built before one hashed to a room shard this
/// replica owns (decision 0020).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_room_create_room_id_attempts",
        "Room IDs built per room creation before one hashed to a room shard this replica owns",
        CREATE_ROOM_ID_ATTEMPTS.clone(),
    );
}
