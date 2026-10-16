//! The registry's once-a-minute pass over its resident rooms' forward extremities: the
//! `hs_room_forward_extremities_*` gauges, a `warn` for a room past [`EXTREMITIES_WARN_AT`],
//! and dummy events into the rooms past `server.rooms.dummy_events_threshold`
//! (`crate::actor::extremities` has the why).
//!
//! As Synapse's `_send_dummy_events_to_fill_extremities`: at most [`DUMMY_ROOMS_PER_PASS`]
//! rooms a pass, the most forked first, one dummy event each, and a room no local user could
//! send one into is left alone for [`DUMMY_EXCLUSION`]. Synapse runs its pass every five
//! minutes; here the pass runs every minute for the gauges and sends dummy events into a room
//! at most once per `server.rooms.dummy_events_interval` (five minutes by default). A dummy
//! event cites at most ten extremities, so a room of 30 is down to three in three of them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hs_kv::KvBackend;
use ruma::OwnedRoomId;
use tokio::time::Instant;

use super::RoomRegistry;
use crate::actor::extremities::DummyOutcome;

/// How often [`RoomRegistry::spawn_extremity_maintainer`]'s pass runs.
pub const EXTREMITY_PASS_INTERVAL: Duration = Duration::from_secs(60);

/// A room with more forward extremities than this is logged at `warn` by the pass, once until
/// it is back under `server.rooms.dummy_events_threshold`: every read of its state is a state
/// resolution of that many forks.
pub const EXTREMITIES_WARN_AT: usize = 50;

/// How many rooms one pass sends dummy events into at most (Synapse's 5).
pub const DUMMY_ROOMS_PER_PASS: usize = 5;

/// How long a room none of this server's joined users could send a dummy event into is left
/// out of the pass before it is tried again. Synapse leaves such a room for a week; an hour
/// here, since trying costs one read of the room's members.
pub const DUMMY_EXCLUSION: Duration = Duration::from_secs(60 * 60);

/// `server.rooms.dummy_events_threshold` until configured: Synapse's `dummy_events_threshold`.
pub const DEFAULT_DUMMY_EVENTS_THRESHOLD: usize = 10;

/// `server.rooms.dummy_events_interval` until configured: Synapse's pass interval.
pub const DEFAULT_DUMMY_EVENTS_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// What the maintainer remembers between passes.
#[derive(Debug, Default)]
pub struct ExtremityMaintainer {
    /// When each room was last sent a dummy event.
    last_dummy: HashMap<OwnedRoomId, Instant>,
    /// Rooms no local user could send a dummy event into, and since when.
    excluded: HashMap<OwnedRoomId, Instant>,
    /// Rooms logged at `warn` for passing [`EXTREMITIES_WARN_AT`], until they recover.
    warned: HashSet<OwnedRoomId>,
}

/// What one pass found and did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtremityPass {
    /// Resident rooms this replica owns whose extremities the pass read (a room busy with another operation is
    /// skipped and read next time).
    pub rooms_read: usize,
    /// The most extremities a room read had.
    pub max_extremities: usize,
    /// Rooms a dummy event was sent into, with how many extremities each had before it.
    pub dummies_sent: Vec<(OwnedRoomId, usize)>,
    /// Rooms no local user could send one into.
    pub no_local_sender: Vec<OwnedRoomId>,
    /// Rooms newly logged at `warn` for passing [`EXTREMITIES_WARN_AT`].
    pub warned: Vec<OwnedRoomId>,
}

impl<B: KvBackend + 'static> RoomRegistry<B> {
    /// Sets how many forward extremities a room may have before the maintainer sends a dummy
    /// event into it (`server.rooms.dummy_events_threshold`); `0` sends none.
    pub fn set_dummy_events_threshold(&self, threshold: usize) {
        self.dummy_events_threshold
            .store(threshold as u64, Ordering::Relaxed);
    }

    /// The dummy event threshold as it stands; `0` off.
    #[must_use]
    pub fn dummy_events_threshold(&self) -> usize {
        usize::try_from(self.dummy_events_threshold.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
    }

    /// Sets the least time between two dummy events into one room
    /// (`server.rooms.dummy_events_interval`).
    pub fn set_dummy_events_interval(&self, interval: Duration) {
        self.dummy_events_interval_ms.store(
            u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// The dummy event interval as it stands.
    #[must_use]
    pub fn dummy_events_interval(&self) -> Duration {
        Duration::from_millis(self.dummy_events_interval_ms.load(Ordering::Relaxed))
    }

    /// Spawns the task that runs [`RoomRegistry::maintain_extremities_pass`] every
    /// [`EXTREMITY_PASS_INTERVAL`]. What `hs serve` installs.
    pub fn spawn_extremity_maintainer(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let mut state = ExtremityMaintainer::default();
            let mut interval = tokio::time::interval(EXTREMITY_PASS_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let pass = registry.maintain_extremities_pass(&mut state).await;
                if !pass.dummies_sent.is_empty() {
                    tracing::info!(
                        rooms = ?pass.dummies_sent,
                        "sent dummy events to merge the forward extremities of rooms past \
                         server.rooms.dummy_events_threshold (hs_room_dummy_events_total)"
                    );
                }
            }
        })
    }

    /// One pass of the maintainer (module docs): reads every resident room's extremity count
    /// without waiting for a room another operation holds, sets the gauges, warns of rooms past
    /// [`EXTREMITIES_WARN_AT`], and sends dummy events into the rooms past the threshold that
    /// are due one.
    pub async fn maintain_extremities_pass(
        &self,
        state: &mut ExtremityMaintainer,
    ) -> ExtremityPass {
        let mut pass = ExtremityPass::default();
        let threshold = self.dummy_events_threshold();
        let interval = self.dummy_events_interval();
        let mut counts: Vec<(OwnedRoomId, usize)> = Vec::new();
        for handle in self.resident_handles().await {
            if let Some(read) = handle
                .try_peek(|actor| (actor.room_id().to_owned(), actor.forward_extremity_count()))
            {
                // A read copy of a room another replica owns is that replica's to count and
                // merge: it could not write a dummy event, and counting it here would count the
                // room twice across the cluster.
                if self.owns_room(&read.0) {
                    counts.push(read);
                }
            }
        }
        pass.rooms_read = counts.len();
        pass.max_extremities = counts.iter().map(|(_, n)| *n).max().unwrap_or(0);
        let just_counts: Vec<usize> = counts.iter().map(|(_, n)| *n).collect();
        crate::metrics::set_forward_extremity_counts(&just_counts);

        let resident: HashSet<&OwnedRoomId> = counts.iter().map(|(id, _)| id).collect();
        state.warned.retain(|id| resident.contains(id));
        for (room_id, n) in &counts {
            if *n > EXTREMITIES_WARN_AT && state.warned.insert(room_id.clone()) {
                tracing::warn!(
                    %room_id,
                    extremities = n,
                    "a room has more than {EXTREMITIES_WARN_AT} forward extremities: every read \
                     of its current state resolves that many forks \
                     (hs_room_forward_extremities_max; see server.rooms.dummy_events_threshold)"
                );
                pass.warned.push(room_id.clone());
            } else if *n <= threshold.max(1) {
                state.warned.remove(room_id);
            }
        }

        if threshold == 0 {
            return pass;
        }
        let now = Instant::now();
        state
            .excluded
            .retain(|_, since| now.duration_since(*since) < DUMMY_EXCLUSION);
        state
            .last_dummy
            .retain(|_, at| now.duration_since(*at) < interval);
        let mut due: Vec<(OwnedRoomId, usize)> = counts
            .into_iter()
            .filter(|(id, n)| {
                *n > threshold
                    && !state.excluded.contains_key(id)
                    && !state.last_dummy.contains_key(id)
            })
            .collect();
        due.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        due.truncate(DUMMY_ROOMS_PER_PASS);
        for (room_id, n) in due {
            let Some(handle) = self.resident_handle(&room_id).await else {
                continue;
            };
            let outcome = handle.send_dummy_event(crate::metrics::now_ms()).await;
            match outcome {
                Ok(DummyOutcome::Sent { .. }) => {
                    state.last_dummy.insert(room_id.clone(), now);
                    pass.dummies_sent.push((room_id, n));
                }
                Ok(DummyOutcome::NotForked) => {}
                Ok(DummyOutcome::NoLocalSender) => {
                    tracing::info!(
                        %room_id,
                        extremities = n,
                        "no joined user of this server may send a dummy event into this room to \
                         merge its forward extremities; trying again in an hour"
                    );
                    state.excluded.insert(room_id.clone(), now);
                    pass.no_local_sender.push(room_id);
                }
                Err(error) => {
                    tracing::warn!(
                        %room_id,
                        extremities = n,
                        %error,
                        "could not send a dummy event to merge this room's forward extremities"
                    );
                    state.last_dummy.insert(room_id, now);
                }
            }
        }
        pass
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::actor::{CreateRoomRequest, RoomActor};
    use crate::identity::HomeserverIdentity;
    use crate::persist::Tables;

    /// A room of alice's on `backend` forked `forks` ways at its head.
    fn forked(backend: &MemoryBackend, forks: usize) -> RoomActor<MemoryBackend> {
        let alice = user_id!("@alice:maint.test").to_owned();
        let mut actor = RoomActor::create_room(
            backend.clone(),
            Tables::open(backend).unwrap(),
            HomeserverIdentity::for_tests("maint.test"),
            alice.clone(),
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                ..Default::default()
            },
            1,
        )
        .unwrap();
        let head: Vec<_> = actor
            .forward_extremity_ids()
            .iter()
            .filter_map(|(id, _)| actor.sn_of(id))
            .collect();
        for n in 0..forks {
            actor
                .send_event_citing(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"msgtype": "m.text", "body": format!("fork {n}")}),
                    None,
                    10 + n as i64,
                    &head,
                )
                .unwrap();
        }
        actor
    }

    /// The pass reads every resident room's extremities into the gauges, warns of a room past
    /// [`EXTREMITIES_WARN_AT`] once, sends a dummy event into a room past the threshold, and
    /// not again into the same room within the interval; a threshold of 0 sends none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pass_measures_warns_and_merges_rooms_past_the_threshold() {
        let backend = MemoryBackend::new();
        let registry =
            RoomRegistry::open(backend.clone(), HomeserverIdentity::for_tests("maint.test"))
                .unwrap();
        let big = forked(&backend, 60);
        let big_id = big.room_id().to_owned();
        let small = forked(&backend, 2);
        let handle = registry.insert(big).await;
        registry.insert(small).await;
        let mut state = ExtremityMaintainer::default();

        registry.set_dummy_events_threshold(0);
        let pass = registry.maintain_extremities_pass(&mut state).await;
        assert_eq!(pass.rooms_read, 2);
        assert_eq!(pass.max_extremities, 60);
        assert_eq!(pass.warned, vec![big_id.clone()]);
        assert!(pass.dummies_sent.is_empty(), "threshold 0 sends nothing");

        registry.set_dummy_events_threshold(DEFAULT_DUMMY_EVENTS_THRESHOLD);
        let pass = registry.maintain_extremities_pass(&mut state).await;
        assert!(pass.warned.is_empty(), "warned once");
        assert_eq!(pass.dummies_sent, vec![(big_id.clone(), 60)]);
        assert_eq!(
            handle.query(|actor| actor.forward_extremity_count()).await,
            60 - 10 + 1
        );

        // Within the interval: not again.
        let pass = registry.maintain_extremities_pass(&mut state).await;
        assert!(pass.dummies_sent.is_empty());
        assert_eq!(pass.max_extremities, 51);

        registry.set_dummy_events_interval(Duration::ZERO);
        let pass = registry.maintain_extremities_pass(&mut state).await;
        assert_eq!(pass.dummies_sent, vec![(big_id.clone(), 51)]);
        assert_eq!(crate::metrics::forward_extremity_rooms("+Inf"), 2);
        assert!(crate::metrics::dummy_events("sent") >= 2);

        // A room another operation holds is skipped, not waited for.
        let busy = handle.clone();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = tokio::spawn(async move {
            busy.query(move |_| {
                let _ = held_tx.send(());
                let _ = release_rx.recv();
            })
            .await;
        });
        held_rx.await.unwrap();
        let pass = registry.maintain_extremities_pass(&mut state).await;
        assert_eq!(pass.rooms_read, 1);
        release_tx.send(()).unwrap();
        holder.await.unwrap();
    }
}
