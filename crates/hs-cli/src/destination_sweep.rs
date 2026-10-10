//! The destination sweep (decision 0042): every hour, forgets the federation destinations this
//! server shares no room with once they have been idle, or failing with a queue only for rooms
//! this server has left, for `federation.forget_unused_destinations_after`; and the room
//! sharing it and the admin API's `federation.destinations.forget`/`.prune` decide by,
//! [`RegistryRoomSharing`], read from the room registry.
//!
//! The setting is hot: `hs serve`'s `federation` applier writes it to the [`Retention`] the
//! sweep reads at each run, and `0` turns the sweep off until it is set again. One sweep logs
//! one line (`swept the federation destinations`: how many were forgotten by reason, how many
//! kept, how long it took); what it forgets is counted in
//! `hs_federation_destinations_forgotten_total{reason,by="sweep"}` by the source.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hs_admin::federation::PruneRules;
use hs_federation::admin_source::DestinationStoreSource;
use hs_federation::room_sharing::{RoomSharing, SharedRoom};
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;

/// How long after start the first sweep runs: the sender has resumed its queues and the
/// first attempts have been made by then, so a destination is judged on this run's facts.
pub const SWEEP_FIRST_AFTER: Duration = Duration::from_secs(5 * 60);

/// How often the sweep runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);

/// `federation.forget_unused_destinations_after` as the sweep reads it: milliseconds, `0` for
/// off. Written by the configuration applier, read at each run.
#[derive(Debug, Default)]
pub struct Retention(AtomicU64);

impl Retention {
    /// Holds `after` (see [`Retention::set`]).
    #[must_use]
    pub fn new(after: Duration) -> Self {
        let retention = Self::default();
        retention.set(after);
        retention
    }

    /// Replaces the retention; the next sweep uses it.
    pub fn set(&self, after: Duration) {
        self.0.store(
            u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }

    /// The retention in force, or `None` when the sweep is off (`0`).
    #[must_use]
    pub fn get(&self) -> Option<Duration> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }
}

/// The [`PruneRules`] a sweep applies for `after`: idle for that long, or failing for that
/// long with a queue only for rooms this server left.
#[must_use]
pub fn rules_for(after: Duration) -> PruneRules {
    PruneRules {
        idle_for: after,
        failing_for: Some(after),
    }
}

/// The running sweep.
pub struct DestinationSweep {
    task: tokio::task::AbortHandle,
}

impl DestinationSweep {
    /// Starts sweeping `source` on the current runtime: the first run after
    /// [`SWEEP_FIRST_AFTER`], then every [`SWEEP_EVERY`], each with the retention `retention`
    /// holds at the time.
    #[must_use]
    pub fn start(source: Arc<DestinationStoreSource>, retention: Arc<Retention>) -> Self {
        let task = tokio::spawn(async move {
            tokio::time::sleep(SWEEP_FIRST_AFTER).await;
            loop {
                sweep_once(&source, &retention).await;
                tokio::time::sleep(SWEEP_EVERY).await;
            }
        })
        .abort_handle();
        Self { task }
    }

    /// Stops the sweep; a run under way is cut short (the source forgets one destination at a
    /// time, each in its own transactions, so nothing is left half done).
    pub fn stop(&self) {
        self.task.abort();
    }
}

/// One sweep of `source` with what `retention` holds now; what it did, or nothing when the
/// sweep is off, when it could not run, or when nothing was forgotten (then at `debug`).
pub async fn sweep_once(
    source: &DestinationStoreSource,
    retention: &Retention,
) -> Option<hs_admin::federation::AdminPruneReport> {
    let after = match retention.get() {
        Some(after) => after,
        None => {
            tracing::debug!(
                "the federation destination sweep is off (forget_unused_destinations_after is 0)"
            );
            return None;
        }
    };
    let started = std::time::Instant::now();
    match source.sweep(rules_for(after)).await {
        Ok(report) => {
            let forgotten = report.forgotten.count;
            let unused = report
                .forgotten
                .by_reason
                .get("unused")
                .copied()
                .unwrap_or(0);
            let failing = report
                .forgotten
                .by_reason
                .get("failing")
                .copied()
                .unwrap_or(0);
            if forgotten > 0 {
                tracing::info!(
                    forgotten,
                    unused,
                    failing,
                    kept = report.kept.count,
                    after_secs = after.as_secs(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    first = ?report
                        .forgotten
                        .servers
                        .iter()
                        .take(8)
                        .map(|e| e.server_name.as_str())
                        .collect::<Vec<_>>(),
                    "swept the federation destinations: forgot the servers this one shares no \
                     room with that were idle, or failing with a queue only for rooms it left"
                );
            } else {
                tracing::debug!(
                    kept = report.kept.count,
                    after_secs = after.as_secs(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "swept the federation destinations; nothing to forget"
                );
            }
            Some(report)
        }
        Err(error) => {
            tracing::warn!(%error, "the federation destination sweep could not run");
            None
        }
    }
}

/// [`RoomSharing`] over the room registry: every room this server holds, with the servers
/// that have a joined member in it. Reads each room once per call (an owned room through its
/// resident actor, another replica's room from the store), so it costs one read per room: the
/// sweep runs hourly, and the admin API's source keeps a reading for a short while.
pub struct RegistryRoomSharing<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
}

impl<B: KvBackend + 'static> RegistryRoomSharing<B> {
    /// Over `rooms`.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>) -> Self {
        Self { rooms }
    }
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> RoomSharing for RegistryRoomSharing<B> {
    async fn shared_rooms(&self) -> Result<Vec<SharedRoom>, String> {
        let room_ids = self
            .rooms
            .list_all_room_ids()
            .map_err(|e| format!("cannot list the rooms: {e}"))?;
        let mut out = Vec::with_capacity(room_ids.len());
        for room_id in room_ids {
            match self
                .rooms
                .read_room(&room_id, |actor| crate::federation::joined_servers(actor))
                .await
            {
                Ok(servers) => out.push(SharedRoom {
                    room_id: room_id.to_string(),
                    servers,
                }),
                // Gone between the listing and now: shares nothing.
                Err(hs_room::error::RoomError::RoomNotFound(_)) => {}
                Err(e) => {
                    return Err(format!("cannot read the members of {room_id}: {e}"));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retention_is_off_at_zero_and_the_rules_follow_it() {
        let retention = Retention::new(Duration::from_secs(7 * 86_400));
        assert_eq!(retention.get(), Some(Duration::from_secs(7 * 86_400)));
        retention.set(Duration::ZERO);
        assert_eq!(retention.get(), None, "0 is off");
        retention.set(Duration::from_secs(60));
        let rules = rules_for(retention.get().unwrap());
        assert_eq!(rules.idle_for, Duration::from_secs(60));
        assert_eq!(rules.failing_for, Some(Duration::from_secs(60)));
    }
}
