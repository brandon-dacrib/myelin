//! [`RoomActor::catch_up`]: advancing a read-only copy of a room by reading only what the store
//! holds past it (`docs/rfcs/0018-room-actor-catch-up.md`, decision 0022).
//!
//! A replica that does not own a room still reads it -- `/sync` on that replica needs the
//! room's events and state -- through a copy built by [`RoomActor::load`]. Before this module the
//! copy was rebuilt whole every time the owner wrote, O(room size) per event. The owner appends
//! to the timeline at consecutive positions, and everything else [`RoomActor::load`] derives
//! from an appended event (its body, its state, the relations index, the forward extremities) is
//! in the store by the time its timeline row is, so a copy can take the rows past its own head
//! and absorb them exactly the way `load`'s timeline loop does.
//!
//! What a copy cannot see in new rows is a change to records it already read: outliers added,
//! history placed below the head (backfill, a gap filled), a gap closed, events purged,
//! extremities pruned. Every such write bumps the room's rewrite counter
//! ([`crate::persist::RoomRewriteKey`]) in the same transaction, and a copy whose counter is
//! behind the store's is told to reload ([`CatchUpReload::Rewritten`]). So is one that finds a
//! position missing ([`CatchUpReload::PositionGap`]), a new event with an explicit state (a
//! rejoin through another server; [`CatchUpReload::ExplicitState`]), or a room that is gone.
//! **A new write path that changes a room's existing rows, or places rows below its head, must
//! call `bump_rewrites` in its transaction too**, or other replicas' copies miss it until
//! they are evicted (decision 0022).
//!
//! Redactions are applied here rather than reloaded for: the redaction is a timeline event, and
//! its target's stored row is rewritten with the `redacted` flag a moment after
//! ([`RoomActor::apply_redaction`]). The copy re-reads the target's row when it absorbs the
//! redaction, and a target whose row does not say `redacted` yet is re-checked on each later
//! catch-up, a bounded number of times ([`PENDING_REDACTION_CHECKS`]): a redaction received over
//! federation is never flagged on the owner either, and the copy must not keep asking.

use std::ops::Bound;

use bytes::Bytes;
use hs_tables::key::TupleKey;
use hs_tables::keyspace::TypedKeyspace;

use super::*;
use crate::persist::TimelineKey;

/// How many catch-ups re-check a redaction target whose stored row did not say `redacted` when
/// the redaction was absorbed, before the copy stops asking. The owner rewrites the row right
/// after it persists the redaction, so one re-check is normally enough.
pub const PENDING_REDACTION_CHECKS: u32 = 16;

/// Why [`RoomActor::catch_up`] could not advance a copy by reading only the new timeline rows,
/// and the copy must be loaded again whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUpReload {
    /// The room's records were changed other than by appending at the head since the copy was
    /// loaded (the rewrite counter moved).
    Rewritten,
    /// The first unread position was not the one after the copy's head, or the new rows skip a
    /// position.
    PositionGap {
        /// The position the copy expected next.
        expected: i64,
        /// The position it found.
        found: i64,
    },
    /// A new event was persisted with an explicit state, or with a timeline gap below it: a
    /// rejoin through another server, which a reload replays the way it was taken.
    ExplicitState {
        /// The event's position.
        room_pos: i64,
    },
    /// A new timeline row names an event whose record is missing, or one the copy already
    /// holds.
    UnexpectedRow {
        /// The row's position.
        room_pos: i64,
    },
    /// The room no longer exists in the store (an administrator deleted it).
    Gone,
}

impl CatchUpReload {
    /// A short label for logs and the `reason` label of a reload counter.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Rewritten => "rewritten",
            Self::PositionGap { .. } => "position_gap",
            Self::ExplicitState { .. } => "explicit_state",
            Self::UnexpectedRow { .. } => "unexpected_row",
            Self::Gone => "gone",
        }
    }
}

impl std::fmt::Display for CatchUpReload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rewritten => write!(f, "the room's records were rewritten since it was read"),
            Self::PositionGap { expected, found } => {
                write!(f, "expected timeline position {expected}, found {found}")
            }
            Self::ExplicitState { room_pos } => write!(
                f,
                "the event at position {room_pos} was taken with an explicit state"
            ),
            Self::UnexpectedRow { room_pos } => {
                write!(
                    f,
                    "the timeline row at position {room_pos} is not a new event"
                )
            }
            Self::Gone => write!(f, "the room no longer exists"),
        }
    }
}

/// What [`RoomActor::catch_up`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUp {
    /// The copy now holds everything the store had when the call read it.
    Advanced {
        /// How many timeline events were read and absorbed (0 when the store was not ahead).
        events: usize,
        /// Redaction targets whose stored row did not say `redacted` yet; while this is not 0
        /// the caller should call again even when the store's head has not moved.
        pending_redactions: usize,
    },
    /// The copy cannot be advanced incrementally; load the room again. It is left as it was.
    Reload(CatchUpReload),
}

/// Bumps `room_sn`'s rewrite counter ([`crate::persist::RoomRewriteKey`]) inside `txn`. Every
/// transaction that changes a room's records other than by appending at the head calls this.
///
/// # Errors
/// Whatever the backend's `atomic_add` returns.
pub(crate) fn bump_rewrites<B: KvBackend>(
    tables: &Tables<B>,
    txn: &mut B::Txn,
    room_sn: RoomSn,
) -> Result<(), hs_kv::KvError> {
    use hs_kv::KvWrite as _;
    txn.atomic_add(tables.rewrites.raw(), &(room_sn,).encode(), 1)
        .map(|_| ())
}

/// `room_sn`'s rewrite counter as `snapshot` sees it; 0 when it has none.
///
/// # Errors
/// [`RoomError::Store`] on a storage failure, [`RoomError::Internal`] for a corrupt value.
pub(crate) fn read_rewrites<B: KvBackend>(
    tables: &Tables<B>,
    snapshot: &B::Snapshot,
    room_sn: RoomSn,
) -> Result<i64, RoomError> {
    match tables.rewrites.get(snapshot, &(room_sn,))? {
        None => Ok(0),
        Some(bytes) => {
            let arr: [u8; 8] = bytes
                .as_ref()
                .try_into()
                .map_err(|_| RoomError::Internal("corrupt room rewrite counter".into()))?;
            Ok(i64::from_be_bytes(arr))
        }
    }
}

impl<B: KvBackend> RoomActor<B> {
    /// The newest timeline position this actor has taken from the store or written itself; 0
    /// for a room with no timeline. What a reader compares with the store's durable head.
    #[must_use]
    pub fn read_position(&self) -> i64 {
        self.next_room_pos - 1
    }

    /// Advances this actor to the store's current head by reading only the timeline rows past
    /// its own (`docs/rfcs/0018-room-actor-catch-up.md`; this module's docs). The events those
    /// rows name are absorbed in timeline order, exactly as [`RoomActor::load`] absorbs them,
    /// so the state at each one -- membership, history visibility -- is the state a fresh load
    /// would compute; the forward extremities are re-read; a redaction among them marks its
    /// target. Reads only: it never writes, so it is safe on a copy of a room another replica
    /// owns, and harmless (a no-op) on the owner.
    ///
    /// Returns [`CatchUp::Reload`], having changed nothing, when the copy cannot be advanced
    /// this way.
    ///
    /// # Errors
    /// [`RoomError::Store`] on a storage failure, [`RoomError::InvalidEvent`] or
    /// [`RoomError::Internal`] for a record that does not decode, [`RoomError::State`] if the
    /// state store fails. After an error the actor may hold part of the batch; the caller
    /// should drop it.
    pub fn catch_up(&mut self) -> Result<CatchUp, RoomError> {
        let snapshot = self.backend.snapshot();
        let room_sn = self.room_sn;
        if self.deleted || self.tables.room_meta.get(&snapshot, &(room_sn,))?.is_none() {
            return Ok(CatchUp::Reload(CatchUpReload::Gone));
        }
        if read_rewrites(&self.tables, &snapshot, room_sn)? != self.rewrites_seen {
            return Ok(CatchUp::Reload(CatchUpReload::Rewritten));
        }

        let start = self.next_room_pos;
        let mut spec = TypedKeyspace::<B::Keyspace, TimelineKey>::prefix(&(room_sn,));
        spec.start = Bound::Included(Bytes::from((room_sn, start).encode()));
        let mut rows: Vec<(i64, EventSn)> = Vec::new();
        for item in self.tables.timeline.range(&snapshot, spec) {
            let ((_, room_pos), value) = item?;
            let sn_bytes: [u8; 8] = value
                .as_ref()
                .try_into()
                .map_err(|_| RoomError::Internal("corrupt timeline entry".into()))?;
            rows.push((room_pos, EventSn::from_be_bytes(sn_bytes)));
        }

        // Everything is checked before anything is absorbed, so a reload verdict leaves the
        // actor exactly as it was.
        let mut batch: Vec<(i64, EventSn, Event, bool)> = Vec::with_capacity(rows.len());
        for (expected, (room_pos, event_sn)) in (start..).zip(rows) {
            if room_pos != expected {
                return Ok(CatchUp::Reload(CatchUpReload::PositionGap {
                    expected,
                    found: room_pos,
                }));
            }
            if self.known(event_sn) {
                return Ok(CatchUp::Reload(CatchUpReload::UnexpectedRow { room_pos }));
            }
            if self
                .tables
                .state_snapshots
                .get(&snapshot, &(room_sn, event_sn))?
                .is_some()
                || self
                    .tables
                    .timeline_gaps
                    .get(&snapshot, &(room_sn, room_pos))?
                    .is_some()
            {
                return Ok(CatchUp::Reload(CatchUpReload::ExplicitState { room_pos }));
            }
            let Some(bytes) = self.tables.events.get(&snapshot, &(event_sn,))? else {
                return Ok(CatchUp::Reload(CatchUpReload::UnexpectedRow { room_pos }));
            };
            let persisted: PersistedEvent =
                serde_json::from_slice(&bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
            let mut event = Event::parse(&persisted.json, self.room_version.clone())?;
            *event.flags_mut() = EventFlags::from_byte(persisted.flags);
            batch.push((room_pos, event_sn, event, persisted.purged));
        }

        // Pending redactions from earlier catch-ups: their rows may say so by now.
        let pending: Vec<(EventSn, u32)> = self
            .pending_redactions
            .iter()
            .map(|(sn, checks)| (*sn, *checks))
            .collect();
        for (sn, checks) in pending {
            if self.take_stored_redaction(&snapshot, sn)? || checks + 1 >= PENDING_REDACTION_CHECKS
            {
                self.pending_redactions.remove(&sn);
            } else {
                self.pending_redactions.insert(sn, checks + 1);
            }
        }

        let events = batch.len();
        let mut redaction_targets: Vec<OwnedEventId> = Vec::new();
        for (room_pos, event_sn, event, purged) in batch {
            if purged {
                self.absorb_purged_event(event_sn, event, room_pos, None)?;
                continue;
            }
            if event.header().event_type == "m.room.redaction"
                && let Some(target) = extract_redacts(&event)
            {
                redaction_targets.push(target);
            }
            self.absorb_loaded_event(event_sn, event, room_pos, None)?;
        }
        for target in redaction_targets {
            let Some(target_sn) = self.sn_of(&target) else {
                continue;
            };
            if !self.take_stored_redaction(&snapshot, target_sn)? {
                self.pending_redactions.insert(target_sn, 0);
            }
        }

        if events > 0 {
            let ext_spec =
                TypedKeyspace::<B::Keyspace, crate::persist::ExtremityKey>::prefix(&(room_sn,));
            let mut extremities = BTreeSet::new();
            for item in self.tables.extremities_fwd.range(&snapshot, ext_spec) {
                let ((_, sn), _) = item?;
                extremities.insert(sn);
            }
            self.forward_extremities = extremities;
        }

        Ok(CatchUp::Advanced {
            events,
            pending_redactions: self.pending_redactions.len(),
        })
    }

    /// Re-reads `sn`'s stored row and, if it says `redacted`, marks the held event so. Answers
    /// whether it did (an event already marked, or not held at all, counts as done).
    fn take_stored_redaction(
        &mut self,
        snapshot: &B::Snapshot,
        sn: EventSn,
    ) -> Result<bool, RoomError> {
        if !self.known(sn) {
            return Ok(true);
        }
        // A copy not resident needs no marking: its next read brings the row as it is.
        let cached_redacted = self
            .cache
            .borrow()
            .peek(sn)
            .is_some_and(|event| event.header().flags.is_redacted());
        if cached_redacted {
            return Ok(true);
        }
        let Some(bytes) = self.tables.events.get(snapshot, &(sn,))? else {
            return Ok(true);
        };
        let persisted: PersistedEvent =
            serde_json::from_slice(&bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
        if !EventFlags::from_byte(persisted.flags).is_redacted() {
            return Ok(false);
        }
        self.update_cached(sn, |event| event.flags_mut().set_redacted(true));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::CreateRoomRequest;
    use hs_kv::memory::MemoryBackend;
    use ruma::{UserId, user_id};
    use serde_json::json;

    /// The owner's actor, and what a reader on another replica needs to load its own copy.
    struct Room {
        owner: RoomActor<MemoryBackend>,
        backend: MemoryBackend,
        tables: Tables<MemoryBackend>,
        identity: HomeserverIdentity,
    }

    impl Room {
        fn new() -> Self {
            let backend = MemoryBackend::new();
            let tables = Tables::open(&backend).unwrap();
            let identity = HomeserverIdentity::for_tests("hs1");
            let owner = RoomActor::create_room(
                backend.clone(),
                tables.clone(),
                identity.clone(),
                user_id!("@alice:hs1").to_owned(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
            Self {
                owner,
                backend,
                tables,
                identity,
            }
        }

        fn load(&self) -> RoomActor<MemoryBackend> {
            RoomActor::load(
                self.backend.clone(),
                self.tables.clone(),
                self.identity.clone(),
                self.owner.room_id(),
            )
            .unwrap()
            .unwrap()
        }

        fn say(&mut self, sender: &UserId, body: &str, ts: i64) -> Event {
            self.owner
                .send_event(
                    sender.to_owned(),
                    "m.room.message".to_owned(),
                    None,
                    json!({"msgtype": "m.text", "body": body}),
                    None,
                    ts,
                )
                .unwrap()
        }
    }

    fn timeline(actor: &RoomActor<MemoryBackend>) -> Vec<(i64, String, bool)> {
        actor
            .events_after(i64::MIN, usize::MAX)
            .into_iter()
            .map(|(pos, e)| {
                (
                    pos,
                    e.event_id().to_string(),
                    e.header().flags.is_redacted(),
                )
            })
            .collect()
    }

    fn state(actor: &RoomActor<MemoryBackend>) -> Vec<String> {
        let mut ids: Vec<String> = actor
            .full_state()
            .unwrap()
            .iter()
            .map(|e| e.event_id().to_string())
            .collect();
        ids.sort();
        ids
    }

    /// What every reader of `copy` could observe is what a fresh load shows: timeline, state,
    /// extremities, and each user's visibility of each event.
    fn assert_same_as_fresh_load(room: &Room, copy: &RoomActor<MemoryBackend>, users: &[&UserId]) {
        let fresh = room.load();
        assert_eq!(timeline(copy), timeline(&fresh));
        assert_eq!(state(copy), state(&fresh));
        assert_eq!(copy.forward_extremities, fresh.forward_extremities);
        assert_eq!(copy.read_position(), fresh.read_position());
        for (_, event) in fresh.events_after(i64::MIN, usize::MAX) {
            let held = copy.event_by_id(event.event_id()).unwrap();
            for user in users {
                assert_eq!(
                    copy.event_visible_to(held, user).unwrap(),
                    fresh.event_visible_to(event, user).unwrap(),
                    "{user} and {}",
                    event.event_id()
                );
            }
        }
    }

    #[test]
    fn a_copy_reads_only_the_new_rows_and_ends_up_as_a_fresh_load_would() {
        let mut room = Room::new();
        let alice = user_id!("@alice:hs1");
        let mut copy = room.load();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Advanced {
                events: 0,
                pending_redactions: 0
            },
            "nothing new"
        );
        let head = copy.read_position();
        for i in 0..5 {
            room.say(alice, &format!("#{i}"), 10 + i);
        }
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Advanced {
                events: 5,
                pending_redactions: 0
            }
        );
        assert_eq!(copy.read_position(), head + 5);
        assert_eq!(copy.read_position(), room.owner.read_position());
        assert_eq!(
            copy.head_update().unwrap().event_id,
            room.owner.head_update().unwrap().event_id
        );
        assert_same_as_fresh_load(&room, &copy, &[alice]);
    }

    /// History visibility and membership changes in one batch are applied in timeline order: a
    /// member sees what the state at each event lets them see, not what the state at the end
    /// would.
    #[test]
    fn visibility_and_membership_changes_in_one_batch_are_applied_in_order() {
        let mut room = Room::new();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let mut copy = room.load();

        room.owner
            .send_event(
                alice.to_owned(),
                "m.room.history_visibility".to_owned(),
                Some(String::new()),
                json!({"history_visibility": "joined"}),
                None,
                10,
            )
            .unwrap();
        let before_join = room.say(alice, "before bob", 11);
        room.owner
            .membership_action(bob.to_owned(), Action::Join, bob.to_owned(), json!({}), 12)
            .unwrap();
        let while_joined = room.say(alice, "while bob is here", 13);
        room.owner
            .membership_action(bob.to_owned(), Action::Leave, bob.to_owned(), json!({}), 14)
            .unwrap();
        let after_leave = room.say(alice, "after bob", 15);

        let CatchUp::Advanced { events, .. } = copy.catch_up().unwrap() else {
            panic!("an ordinary batch is caught up, not reloaded");
        };
        assert_eq!(events, 6);
        let visible = |event: &Event| {
            copy.event_visible_to(copy.event_by_id(event.event_id()).unwrap(), bob)
                .unwrap()
        };
        assert!(
            !visible(&before_join),
            "sent before bob joined a `joined` room"
        );
        assert!(visible(&while_joined));
        assert!(!visible(&after_leave), "sent after bob left");
        assert_same_as_fresh_load(&room, &copy, &[alice, bob]);
    }

    #[test]
    fn a_redaction_marks_its_target_now_or_on_a_later_catch_up() {
        let mut room = Room::new();
        let alice = user_id!("@alice:hs1");
        let mut copy = room.load();

        // Through the client API: the target's row says `redacted` by the time anyone reads.
        let first = room.say(alice, "regret", 10);
        room.owner
            .redact_txn(
                alice.to_owned(),
                None,
                "t1",
                first.event_id().to_owned(),
                None,
                11,
            )
            .unwrap();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Advanced {
                events: 2,
                pending_redactions: 0
            }
        );
        assert!(
            copy.event_by_id(first.event_id())
                .unwrap()
                .header()
                .flags
                .is_redacted()
        );

        // The redaction event is in the store a moment before its target's row is rewritten:
        // a catch-up in between remembers the target and picks the flag up next time.
        let second = room.say(alice, "regret again", 12);
        room.owner
            .send_event(
                alice.to_owned(),
                "m.room.redaction".to_owned(),
                None,
                json!({}),
                Some(second.event_id().to_owned()),
                13,
            )
            .unwrap();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Advanced {
                events: 2,
                pending_redactions: 1
            }
        );
        assert!(
            !copy
                .event_by_id(second.event_id())
                .unwrap()
                .header()
                .flags
                .is_redacted()
        );
        room.owner.apply_redaction(second.event_id()).unwrap();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Advanced {
                events: 0,
                pending_redactions: 0
            }
        );
        assert_same_as_fresh_load(&room, &copy, &[alice]);
    }

    #[test]
    fn a_target_never_flagged_stops_being_checked() {
        let mut room = Room::new();
        let alice = user_id!("@alice:hs1");
        let mut copy = room.load();
        let target = room.say(alice, "never flagged", 10);
        room.owner
            .send_event(
                alice.to_owned(),
                "m.room.redaction".to_owned(),
                None,
                json!({}),
                Some(target.event_id().to_owned()),
                11,
            )
            .unwrap();
        let mut pending = Vec::new();
        for _ in 0..=PENDING_REDACTION_CHECKS {
            let CatchUp::Advanced {
                pending_redactions, ..
            } = copy.catch_up().unwrap()
            else {
                panic!("not a reload");
            };
            pending.push(pending_redactions);
        }
        assert_eq!(pending.first(), Some(&1));
        assert_eq!(pending.last(), Some(&0), "{pending:?}");
    }

    #[test]
    fn a_purge_a_deletion_and_a_missing_position_each_ask_for_a_reload() {
        let mut room = Room::new();
        let alice = user_id!("@alice:hs1");
        for i in 0..4 {
            room.say(alice, &format!("old #{i}"), 10 + i);
        }
        let mut copy = room.load();
        let before = timeline(&copy);

        // A purge rewrites rows the copy already read.
        let plan = room.owner.purge_plan(Some(100), None, true).unwrap();
        assert!(!plan.positions.is_empty());
        room.owner.purge_positions(&plan.positions).unwrap();
        room.say(alice, "after the purge", 200);
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Reload(CatchUpReload::Rewritten)
        );
        assert_eq!(timeline(&copy), before, "a reload verdict changes nothing");
        let mut copy = room.load();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Advanced {
                events: 0,
                pending_redactions: 0
            },
            "a fresh copy has the counter it was loaded with"
        );

        // A row past the head that skips a position.
        let skip_to = copy.read_position() + 3;
        let sn = copy.timeline.values().next_back().copied().unwrap();
        let room_sn = copy.room_sn;
        transact(&room.backend, TransactConfig::default(), |txn| {
            room.tables
                .timeline
                .put(txn, &(room_sn, skip_to), &sn.to_be_bytes())
                .map_err(to_kv)
        })
        .unwrap();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Reload(CatchUpReload::PositionGap {
                expected: skip_to - 2,
                found: skip_to
            })
        );

        // A deleted room.
        room.owner.delete_everything().unwrap();
        assert_eq!(
            copy.catch_up().unwrap(),
            CatchUp::Reload(CatchUpReload::Gone)
        );
        assert_eq!(CatchUpReload::Gone.reason(), "gone");
    }
}
