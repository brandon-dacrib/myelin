//! What the session hub ([`crate::hub::SessionHub`]) is doing with the room stream: which
//! updates it has received and not finished, which step each update in hand is at and for how
//! long, and the consumed mark that follows from that.
//!
//! # Why this exists
//!
//! On 2026-10-14 the demo's hub stopped writing feeds for half an hour: one update's room work
//! did not finish, and the hub, which fed one update at a time, fed nothing else after it. The
//! only signs were `/sync`'s "did not catch up" warning, every request, with the same
//! `consumed` number, and a fan-out counter that stopped moving; nothing said which room, or
//! where in the update the hub was waiting. The hub now feeds each room's updates in order on a
//! worker of its own ([`crate::hub`]'s module docs, "One room at a time, many rooms at once"),
//! and this module is how it keeps count across the workers and tells an operator when one of
//! them is stuck:
//!
//! - The consumed mark is the highest stream number below which every received update is
//!   finished ([`Progress::finished`]): an update still in hand holds it, however many later
//!   ones are done, so `/sync`'s read-your-writes wait keeps meaning what it meant.
//! - A read does not wait for an update the hub has had for longer than the stall threshold
//!   ([`Progress::settled_through`]): it is stuck, and waiting for it only delays every
//!   `/sync` of every user by the whole read budget.
//! - Each update in hand records its step ([`InHand::step`]); the hub's watchdog warns, naming
//!   the room, the update and the step, when a step has not moved for
//!   [`STUCK_WARN_AFTER`], and sets the `hs_user_hub_*` gauges.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ruma::{OwnedRoomId, RoomId};
use tokio::time::Instant;

/// How long a room update may be in the hub's hands, from the moment it was received, before a
/// read stops waiting for it ([`Progress::settled_through`]). A normal update takes
/// milliseconds; one of a big room, seconds.
pub const STALL_AFTER: Duration = Duration::from_secs(5);

/// How long one step of one room update may last before the hub's watchdog warns about it.
pub const STUCK_WARN_AFTER: Duration = Duration::from_secs(30);

/// How often the watchdog repeats its warning about an update that is still stuck.
pub const STUCK_WARN_REPEAT: Duration = Duration::from_secs(60);

/// An update the hub has received and not finished.
#[derive(Debug, Clone)]
struct Pending {
    room_id: OwnedRoomId,
    received: Instant,
}

/// An update a hub worker is working on.
#[derive(Debug, Clone)]
struct Active {
    room_id: OwnedRoomId,
    global_seq: u64,
    started: Instant,
    step: &'static str,
    step_since: Instant,
    warned_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The highest stream number received.
    max_received: u64,
    /// Received and not finished, by stream number. Re-reads after a lag carry no number and
    /// are never here.
    pending: BTreeMap<u64, Pending>,
    /// In hand, by [`InHand`] id.
    active: HashMap<u64, Active>,
    /// When the watchdog last warned that updates were waiting for a worker.
    waiting_warned_at: Option<Instant>,
}

/// The hub's count of the room stream. See the module docs.
#[derive(Debug)]
pub(crate) struct Progress {
    inner: Mutex<Inner>,
    /// Bumped on every receive and finish, for a read waiting on [`Progress::settled_through`].
    changed: tokio::sync::watch::Sender<u64>,
    next_id: AtomicU64,
    /// [`STALL_AFTER`], in milliseconds; a test lowers it.
    stall_after_ms: AtomicU64,
    /// [`STUCK_WARN_AFTER`], in milliseconds; a test lowers it.
    stuck_warn_after_ms: AtomicU64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            changed: tokio::sync::watch::channel(0).0,
            next_id: AtomicU64::new(1),
            stall_after_ms: AtomicU64::new(millis(STALL_AFTER)),
            stuck_warn_after_ms: AtomicU64::new(millis(STUCK_WARN_AFTER)),
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl Progress {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Sets the stall threshold ([`STALL_AFTER`]) and the stuck-step warning threshold
    /// ([`STUCK_WARN_AFTER`]).
    pub(crate) fn set_thresholds(&self, stall_after: Duration, stuck_warn_after: Duration) {
        self.stall_after_ms
            .store(millis(stall_after), Ordering::Relaxed);
        self.stuck_warn_after_ms
            .store(millis(stuck_warn_after), Ordering::Relaxed);
    }

    pub(crate) fn stall_after(&self) -> Duration {
        Duration::from_millis(self.stall_after_ms.load(Ordering::Relaxed))
    }

    pub(crate) fn stuck_warn_after(&self) -> Duration {
        Duration::from_millis(self.stuck_warn_after_ms.load(Ordering::Relaxed))
    }

    /// Records that update `seq` of `room_id` has been received. `0` (a re-read after a lag)
    /// is not counted.
    pub(crate) fn received(&self, seq: u64, room_id: &RoomId) {
        if seq == 0 {
            return;
        }
        {
            let mut inner = self.lock();
            inner.max_received = inner.max_received.max(seq);
            inner.pending.insert(
                seq,
                Pending {
                    room_id: room_id.to_owned(),
                    received: Instant::now(),
                },
            );
        }
        self.bump();
    }

    /// Records that update `seq` is finished (fed, or failed and logged) and returns the
    /// consumed mark: the highest number below which every received update is finished.
    /// Numbers the hub never received (skipped by a lag) do not hold it.
    pub(crate) fn finished(&self, seq: u64) -> u64 {
        let mark = {
            let mut inner = self.lock();
            if seq != 0 {
                inner.pending.remove(&seq);
            }
            Self::mark(&inner)
        };
        self.bump();
        mark
    }

    fn mark(inner: &Inner) -> u64 {
        inner
            .pending
            .keys()
            .next()
            .map_or(inner.max_received, |oldest| oldest - 1)
    }

    /// The consumed mark as it stands.
    #[cfg(test)]
    fn consumed(&self) -> u64 {
        Self::mark(&self.lock())
    }

    /// Whether a read that wants everything up to `seq` may go ahead: every update numbered up
    /// to it has been received, and each one not finished has been in the hub's hands for
    /// longer than the stall threshold. `false` while anything up to `seq` has not arrived or
    /// is merely in progress.
    pub(crate) fn settled_through(&self, seq: u64) -> bool {
        let stall_after = self.stall_after();
        let now = Instant::now();
        let inner = self.lock();
        inner.max_received >= seq
            && inner
                .pending
                .range(..=seq)
                .all(|(_, p)| now.duration_since(p.received) >= stall_after)
    }

    /// A receiver that changes on every receive and finish.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Starts tracking one update in hand: `room_id`'s update `global_seq` (`0` for a re-read
    /// or a direct call that carries no number). The returned guard records its steps, and
    /// stops tracking it when dropped.
    pub(crate) fn begin(&self, room_id: &RoomId, global_seq: u64) -> InHand<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        self.lock().active.insert(
            id,
            Active {
                room_id: room_id.to_owned(),
                global_seq,
                started: now,
                step: "starting",
                step_since: now,
                warned_at: None,
            },
        );
        InHand { progress: self, id }
    }

    /// The hub's state for an operator: see [`HubStatus`].
    pub(crate) fn status(&self, published: u64) -> HubStatus {
        let now = Instant::now();
        let stuck_after = self.stuck_warn_after();
        let inner = self.lock();
        let mut in_hand: Vec<UpdateInHand> = inner
            .active
            .values()
            .map(|a| UpdateInHand {
                room_id: a.room_id.clone(),
                global_seq: a.global_seq,
                step: a.step,
                step_for: now.duration_since(a.step_since),
                in_hand_for: now.duration_since(a.started),
            })
            .collect();
        in_hand.sort_by_key(|u| std::cmp::Reverse(u.step_for));
        let stuck = in_hand.iter().filter(|u| u.step_for >= stuck_after).count();
        HubStatus {
            published,
            consumed: Self::mark(&inner),
            received: inner.max_received,
            pending: inner.pending.len(),
            oldest_pending: inner.pending.iter().next().map(|(seq, p)| OldestPending {
                room_id: p.room_id.clone(),
                global_seq: *seq,
                waiting_for: now.duration_since(p.received),
            }),
            stuck,
            in_hand,
        }
    }

    /// What the watchdog should warn about now: every update in hand whose step has lasted at
    /// least the stuck-step threshold and that has not been warned about within
    /// [`STUCK_WARN_REPEAT`], each with how many later updates of its room wait behind it; and,
    /// once a [`STUCK_WARN_REPEAT`], the oldest received update if it has waited that long and
    /// no worker has it (every worker is busy).
    pub(crate) fn due_warnings(&self) -> Warnings {
        let now = Instant::now();
        let after = self.stuck_warn_after();
        let mut inner = self.lock();
        let mut queued_per_room: HashMap<OwnedRoomId, usize> = HashMap::new();
        for p in inner.pending.values() {
            *queued_per_room.entry(p.room_id.clone()).or_default() += 1;
        }
        let mut stuck = Vec::new();
        for a in inner.active.values_mut() {
            let step_for = now.duration_since(a.step_since);
            if step_for < after {
                continue;
            }
            if a.warned_at
                .is_some_and(|at| now.duration_since(at) < STUCK_WARN_REPEAT)
            {
                continue;
            }
            a.warned_at = Some(now);
            let pending_in_room = queued_per_room.get(&a.room_id).copied().unwrap_or(0);
            stuck.push(StuckUpdate {
                room_id: a.room_id.clone(),
                global_seq: a.global_seq,
                step: a.step,
                step_for,
                in_hand_for: now.duration_since(a.started),
                // The update itself is among the room's pending ones, when it carries a number.
                queued_behind: pending_in_room.saturating_sub(usize::from(a.global_seq != 0)),
            });
        }
        let mut waiting = None;
        if let Some((seq, oldest)) = inner.pending.iter().next() {
            let waited = now.duration_since(oldest.received);
            let in_hand = inner.active.values().any(|a| a.room_id == oldest.room_id);
            let due = inner
                .waiting_warned_at
                .is_none_or(|at| now.duration_since(at) >= STUCK_WARN_REPEAT);
            if !in_hand && waited >= after && due {
                waiting = Some(OldestPending {
                    room_id: oldest.room_id.clone(),
                    global_seq: *seq,
                    waiting_for: waited,
                });
            }
        }
        if waiting.is_some() {
            inner.waiting_warned_at = Some(now);
        }
        Warnings { stuck, waiting }
    }
}

/// One update in a worker's hands, tracked until dropped. See [`Progress::begin`].
pub(crate) struct InHand<'a> {
    progress: &'a Progress,
    id: u64,
}

impl InHand<'_> {
    /// Records that the update has reached `step` (a short phrase: what it is waiting for).
    pub(crate) fn step(&self, step: &'static str) {
        if let Some(active) = self.progress.lock().active.get_mut(&self.id) {
            if active.warned_at.is_some() {
                tracing::info!(
                    room_id = %active.room_id,
                    global_seq = active.global_seq,
                    from = active.step,
                    to = step,
                    after_secs = active.step_since.elapsed().as_secs(),
                    "a room update the session hub was stuck on moved on"
                );
                active.warned_at = None;
            }
            active.step = step;
            active.step_since = Instant::now();
        }
    }
}

impl Drop for InHand<'_> {
    fn drop(&mut self) {
        let removed = self.progress.lock().active.remove(&self.id);
        if let Some(active) = removed
            && active.warned_at.is_some()
        {
            tracing::info!(
                room_id = %active.room_id,
                global_seq = active.global_seq,
                step = active.step,
                in_hand_secs = active.started.elapsed().as_secs(),
                "a room update the session hub was stuck on is done"
            );
        }
    }
}

/// What [`Progress::due_warnings`] found.
#[derive(Debug, Default)]
pub(crate) struct Warnings {
    pub(crate) stuck: Vec<StuckUpdate>,
    pub(crate) waiting: Option<OldestPending>,
}

/// One update whose step has not moved for the stuck-step threshold.
#[derive(Debug, Clone)]
pub(crate) struct StuckUpdate {
    pub(crate) room_id: OwnedRoomId,
    pub(crate) global_seq: u64,
    pub(crate) step: &'static str,
    pub(crate) step_for: Duration,
    pub(crate) in_hand_for: Duration,
    pub(crate) queued_behind: usize,
}

/// The session hub's state, for an operator and for health checks
/// ([`crate::hub::SessionHub::status`]).
#[derive(Debug, Clone)]
pub struct HubStatus {
    /// The newest update number the room registry has published.
    pub published: u64,
    /// The consumed mark: every update numbered up to it is fed (or failed and logged).
    pub consumed: u64,
    /// The newest update number the hub has received.
    pub received: u64,
    /// Updates received and not finished.
    pub pending: usize,
    /// The oldest update received and not finished, if any.
    pub oldest_pending: Option<OldestPending>,
    /// How many of [`HubStatus::in_hand`] have been at their step for longer than the
    /// stuck-step threshold ([`STUCK_WARN_AFTER`]).
    pub stuck: usize,
    /// The updates the hub's workers are working on, the longest at its step first.
    pub in_hand: Vec<UpdateInHand>,
}

impl HubStatus {
    /// `published` minus `consumed`: how many updates the feeds are behind the rooms.
    #[must_use]
    pub fn lag(&self) -> u64 {
        self.published.saturating_sub(self.consumed)
    }
}

/// The oldest update the hub has received and not finished.
#[derive(Debug, Clone)]
pub struct OldestPending {
    /// Its room.
    pub room_id: OwnedRoomId,
    /// Its number on the room stream.
    pub global_seq: u64,
    /// How long ago the hub received it.
    pub waiting_for: Duration,
}

/// One update a hub worker is working on.
#[derive(Debug, Clone)]
pub struct UpdateInHand {
    /// Its room.
    pub room_id: OwnedRoomId,
    /// Its number on the room stream; `0` for a re-read after a lag or a direct call.
    pub global_seq: u64,
    /// What it is doing (and so, when stuck, what it is waiting for).
    pub step: &'static str,
    /// How long it has been at that step.
    pub step_for: Duration,
    /// How long a worker has had it.
    pub in_hand_for: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::room_id;

    #[tokio::test]
    async fn the_mark_is_held_by_the_oldest_unfinished_update_and_skips_numbers_never_received() {
        let p = Progress::default();
        let (a, b) = (room_id!("!a:x"), room_id!("!b:x"));
        p.received(1, a);
        p.received(2, b);
        p.received(3, b);
        assert_eq!(p.finished(2), 0, "1 still in hand");
        assert_eq!(p.finished(3), 0);
        assert_eq!(p.finished(1), 3, "everything up to 3 is done");
        // 4 and 5 were skipped by a lag: never received, they hold nothing.
        p.received(6, a);
        assert_eq!(p.consumed(), 5);
        assert_eq!(p.finished(6), 6);
        assert_eq!(p.finished(0), 6, "a re-read carries no number");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_stops_waiting_for_an_update_once_it_has_been_in_hand_past_the_threshold() {
        let p = Progress::default();
        p.set_thresholds(Duration::from_secs(5), Duration::from_secs(30));
        let a = room_id!("!a:x");
        assert!(!p.settled_through(1), "not received yet");
        p.received(1, a);
        assert!(!p.settled_through(1), "in progress");
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(p.settled_through(1), "stalled: not worth waiting for");
        p.received(2, a);
        assert!(!p.settled_through(2), "2 is new");
        assert!(p.settled_through(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_that_does_not_move_is_warned_about_once_a_repeat_naming_its_room() {
        let p = Progress::default();
        p.set_thresholds(Duration::from_secs(5), Duration::from_secs(30));
        let (a, b) = (room_id!("!a:x"), room_id!("!b:x"));
        p.received(7, a);
        p.received(8, a);
        p.received(9, b);
        let in_hand = p.begin(a, 7);
        in_hand.step("reading the room's members and directory entry");
        assert!(p.due_warnings().stuck.is_empty());
        tokio::time::advance(Duration::from_secs(31)).await;
        let warnings = p.due_warnings();
        assert_eq!(warnings.stuck.len(), 1);
        let stuck = &warnings.stuck[0];
        assert_eq!(stuck.room_id, a);
        assert_eq!(stuck.global_seq, 7);
        assert_eq!(stuck.step, "reading the room's members and directory entry");
        assert_eq!(stuck.queued_behind, 1, "8 waits behind it");
        assert!(p.due_warnings().stuck.is_empty(), "not again at once");
        tokio::time::advance(STUCK_WARN_REPEAT).await;
        assert_eq!(p.due_warnings().stuck.len(), 1, "again a repeat later");
        let status = p.status(9);
        assert_eq!(status.consumed, 6);
        assert_eq!(status.lag(), 3);
        assert_eq!(status.stuck, 1);
        assert_eq!(status.in_hand[0].room_id, a);
        drop(in_hand);
        assert!(p.status(9).in_hand.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn updates_waiting_for_a_worker_are_warned_about() {
        let p = Progress::default();
        p.set_thresholds(Duration::from_secs(5), Duration::from_secs(30));
        let (a, b) = (room_id!("!a:x"), room_id!("!b:x"));
        p.received(1, b);
        let _busy = p.begin(a, 0);
        tokio::time::advance(Duration::from_secs(31)).await;
        let warnings = p.due_warnings();
        let waiting = warnings.waiting.expect("1 has waited with no worker on it");
        assert_eq!(waiting.room_id, b);
        assert!(p.due_warnings().waiting.is_none(), "once a repeat");
    }
}
