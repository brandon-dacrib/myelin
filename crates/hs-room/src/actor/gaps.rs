//! The gap a leave and a rejoin leave in the middle of a room's timeline, and how it is filled.
//!
//! When the last user of this server leaves a room hosted elsewhere, this server stops
//! receiving the room's events; its copy stays as it was. A rejoin goes through a server that
//! is in the room (`RoomActor::servers_to_join_through`), and its answer is applied to the
//! copy held here with the room's current state as an explicit snapshot
//! (`RoomActor::accept_remote_join_with_state`). The current state comes back that way; the
//! *timeline* between the leave and the rejoin does not. Timeline positions are a stream order
//! assigned here, so nothing would ever place what happened meanwhile between the two.
//!
//! # The mechanism
//! An event taken with an explicit state whose `prev_events` are not all in this room's
//! timeline, while the timeline holds something already, **opens a gap**
//! (`RoomActor::persist_with`): it is placed [`crate::timeline::TIMELINE_GAP_SPAN`] positions
//! beyond where it would otherwise go, and the positions skipped are recorded, durably and in
//! the same transaction, as a gap below it (`Tables::timeline_gaps`, keyed by the event's own
//! position, its *top*; the gap's *below* is the newest position held before it). What the gap
//! lacks is tracked as the set of event IDs its events cite as `prev_events` and this server
//! does not hold in the timeline -- at first, the rejoin's own `prev_events`.
//!
//! A backward `/messages` page that would walk across an open gap stops at it instead
//! ([`Page::gap`]), and `crate::routes::query::get_messages` asks `crate::backfill::Backfill::
//! fill_gap` to fetch it: a `/backfill` from the missing events, verified as any inbound PDU is,
//! handed to [`RoomActor::accept_gap_events`]. That places the batch *inside* the gap -- newest
//! just below the lowest position filled so far, oldest furthest down, the same newest-first
//! order and the same state walk as history before the oldest held event
//! ([`RoomActor::accept_backfilled_events`]) -- so a backward page from the rejoin walks
//! straight through what was missed and on into what was held before the leave. Events older
//! than the gap's `below` event (by `depth`) are not part of it: the resident's `/backfill`
//! keeps walking past the leave, and what it finds there is either held already or history from
//! before this server's first join, which belongs below the timeline, not in the middle of it.
//! A gap is **closed** when nothing it cites is missing any more, when the server asked had
//! nothing new, or when its positions run out; a page then walks across it.
//!
//! # History, not news
//! Exactly as for history before the oldest held event: nothing is published, the forward
//! extremities and the joined-rooms index are untouched, and
//! [`RoomActor::events_after`] -- what a follower with a cursor reads (appservice delivery) --
//! never returns an event at a position inside a gap, filled or not, open or closed.
//! `/sync`'s own forward reads start at a token issued after the rejoin, above every gap.
//!
//! # Durability
//! The rows say where each gap is and whether it is closed; how far it has been filled (the
//! lowest position held inside it) and what it still lacks are read off the timeline on load
//! ([`RoomActor::restore_gaps`]), so a crash between placing a batch and recording its effect on
//! the gap loses nothing.
//!
//! [`Page::gap`]: super::Page::gap

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use ruma::OwnedEventId;

use super::{PlannedHistory, RoomActor, to_kv, topological_order};
use crate::backfill::{GapAnchor, GapFill};
use crate::error::RoomError;
use crate::persist::TimelineGapRecord;
use crate::pipeline;

/// How many missing event IDs one fetch names as `v` on its `/backfill` request. A gap almost
/// always lacks one (the room's history is a line); a fork cites more, and a URL has a length.
const MAX_FETCH_FROM: usize = 20;

/// One gap, as the actor holds it in memory.
#[derive(Debug, Clone)]
pub(crate) struct Gap {
    /// The newest timeline position held when the gap opened. Gap positions are strictly above
    /// it and strictly below the gap's top.
    below: i64,
    /// The lowest position held in `(below, top]`: the top itself until something is placed.
    filled_to: i64,
    /// Event IDs the gap's events (its top included) cite as `prev_events` that are not in the
    /// timeline: where the next fetch walks back from.
    missing: BTreeSet<OwnedEventId>,
    /// Nothing more will be fetched for it.
    closed: bool,
}

/// A gap in a room's timeline, as [`RoomActor::timeline_gaps`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineGap {
    /// The position of the event the gap sits below (the rejoin).
    pub top: i64,
    /// The newest position held when the gap opened (the leave, usually).
    pub below: i64,
    /// The lowest position filled so far: `top` until something has been placed.
    pub filled_to: i64,
    /// Event IDs the gap still lacks: cited as `prev_events` by what is held of it, and not in
    /// the timeline. Empty once closed.
    pub missing: Vec<OwnedEventId>,
    /// Whether anything more will be fetched for it.
    pub closed: bool,
}

impl<B: KvBackend> RoomActor<B> {
    /// Every `EventSn` placed in the timeline.
    fn timeline_sns(&self) -> HashSet<EventSn> {
        self.timeline.values().copied().collect()
    }

    /// The `prev_events` of `event` that are not in the timeline (held as outliers, or not held
    /// at all).
    fn prevs_not_in_timeline(
        &self,
        event: &Event,
        in_timeline: &HashSet<EventSn>,
    ) -> Vec<OwnedEventId> {
        pipeline::decode_event_ids(event.json().get("prev_events"))
            .into_iter()
            .filter(|id| {
                !self
                    .event_id_index
                    .get(id)
                    .is_some_and(|sn| in_timeline.contains(sn))
            })
            .collect()
    }

    /// Where a gap below `event` would begin -- the newest position held -- if persisting it
    /// opens one: the timeline holds something, and `event` cites a `prev_event` that is not in
    /// it. `None` otherwise (a room's first timeline event, whose earlier history is fetched
    /// below the timeline instead; or an event whose ancestors are all here).
    pub(super) fn gap_below_for(&self, event: &Event) -> Option<i64> {
        let newest = *self.timeline.keys().next_back()?;
        let in_timeline = self.timeline_sns();
        (!self.prevs_not_in_timeline(event, &in_timeline).is_empty()).then_some(newest)
    }

    /// Records the gap `RoomActor::persist_with` just opened below the event at `top`.
    pub(super) fn open_gap(&mut self, top: i64, below: i64, event: &Event) {
        let in_timeline = self.timeline_sns();
        let missing: BTreeSet<OwnedEventId> = self
            .prevs_not_in_timeline(event, &in_timeline)
            .into_iter()
            .collect();
        tracing::info!(
            room_id = %self.room_id,
            event_id = %event.event_id(),
            below,
            top,
            missing = missing.len(),
            "this server was out of the room for part of its history; a gap is open below the event that brought it back"
        );
        self.gaps.insert(
            top,
            Gap {
                below,
                filled_to: top,
                missing,
                closed: false,
            },
        );
    }

    /// Rebuilds the in-memory gaps on load from their rows and the timeline just replayed.
    pub(super) fn restore_gaps(&mut self, rows: Vec<(i64, TimelineGapRecord)>) {
        let in_timeline = self.timeline_sns();
        for (top, record) in rows {
            let filled_to = self
                .timeline
                .range(record.below + 1..=top)
                .next()
                .map_or(top, |(pos, _)| *pos);
            let missing: BTreeSet<OwnedEventId> = if record.closed {
                BTreeSet::new()
            } else {
                self.timeline
                    .range(filled_to..=top)
                    .filter_map(|(_, sn)| self.events.get(sn))
                    .flat_map(|event| self.prevs_not_in_timeline(event, &in_timeline))
                    .collect()
            };
            let closed = record.closed || missing.is_empty();
            self.gaps.insert(
                top,
                Gap {
                    below: record.below,
                    filled_to,
                    missing,
                    closed,
                },
            );
        }
    }

    /// The gap `room_pos` lies inside, by its top: `Some` for a position strictly between a
    /// gap's `below` and its top, filled or not, open or closed. Such a position is history.
    pub(crate) fn gap_containing(&self, room_pos: i64) -> Option<i64> {
        let (top, gap) = self
            .gaps
            .range((
                std::ops::Bound::Excluded(room_pos),
                std::ops::Bound::Unbounded,
            ))
            .next()?;
        (gap.below < room_pos).then_some(*top)
    }

    /// The open gap a backward walk from `start` (exclusive) reaches first, as `(top, below)`:
    /// the walk must not go below `below` without that gap being filled or given up on.
    pub(super) fn gap_barrier(&self, start: i64) -> Option<(i64, i64)> {
        self.gaps
            .iter()
            .filter(|(_, gap)| !gap.closed && gap.below < start.saturating_sub(1))
            .map(|(top, gap)| (*top, gap.below))
            .max_by_key(|(_, below)| *below)
    }

    /// The room's timeline gaps, lowest first. Empty for a room this server has never left and
    /// come back to through another server.
    #[must_use]
    pub fn timeline_gaps(&self) -> Vec<TimelineGap> {
        self.gaps
            .iter()
            .map(|(top, gap)| TimelineGap {
                top: *top,
                below: gap.below,
                filled_to: gap.filled_to,
                missing: gap.missing.iter().cloned().collect(),
                closed: gap.closed,
            })
            .collect()
    }

    /// What `crate::backfill` needs to fill the gap below position `top`: the events it lacks,
    /// to walk back from, and the servers to ask (the same as for history before the oldest
    /// held event). `None` when there is no open gap there.
    #[must_use]
    pub fn gap_anchor(&self, top: i64) -> Option<GapAnchor> {
        let gap = self.gaps.get(&top).filter(|gap| !gap.closed)?;
        Some(GapAnchor {
            top,
            from: gap.missing.iter().take(MAX_FETCH_FROM).cloned().collect(),
            servers: self.servers_to_ask_for_history(),
        })
    }

    /// Stores one batch of the history a gap lacks, as fetched from another server by
    /// `crate::backfill` and verified by its caller (hashes and signatures; nothing here checks
    /// either, and no authorization runs -- the same trust as
    /// [`RoomActor::accept_backfilled_events`] and for the same reasons).
    ///
    /// # Where the events go
    /// Inside the gap below position `top`: the newest of the batch just below the lowest
    /// position filled so far, the oldest furthest down, ordered by `depth`, then
    /// `origin_server_ts`, then event ID. Skipped: events for another room, events already in
    /// the timeline, events with a `depth` above the top's (not history of it), and events no
    /// deeper than the event the gap sits above (`below`'s) -- history from before the leave,
    /// which is either held already or older than this server's first join and belongs below
    /// the timeline. Outliers -- the state the rejoin brought, a rename made while this server
    /// was out -- are placed and otherwise left as they are. When the batch is larger than the
    /// positions left, the newest that fit are placed and the gap is closed.
    ///
    /// # The state at each event
    /// Walked back from the state before the lowest filled event (the rejoin's own explicit
    /// snapshot, for the first batch), exactly as [`RoomActor::accept_backfilled_events`] does,
    /// except that a key the batch holds no earlier setting for reverts to its value in the
    /// state after the `below` event (the room as this server last had it) rather than to
    /// nothing. Exact when the gap is filled in one batch and the room's history is linear.
    ///
    /// # Closing
    /// The gap is closed, durably, when every event it cites is in the timeline, when the
    /// batch held nothing new, or when its positions ran out.
    ///
    /// # What does not happen
    /// No [`crate::protocol::RoomUpdate`] is published, and neither the forward extremities
    /// nor the joined-rooms index change: this is history, not news.
    ///
    /// # Errors
    /// [`RoomError::Internal`] if there is no gap below `top`; [`RoomError::Store`],
    /// [`RoomError::Fenced`] or [`RoomError::State`] from persistence.
    pub fn accept_gap_events(
        &mut self,
        top: i64,
        events: Vec<Event>,
    ) -> Result<GapFill, RoomError> {
        let Some(gap) = self.gaps.get(&top).cloned() else {
            return Err(RoomError::Internal(format!(
                "no timeline gap below position {top} in {}",
                self.room_id
            )));
        };
        if gap.closed {
            return Ok(GapFill {
                added: 0,
                closed: true,
            });
        }
        let depth_at = |pos: i64| -> Option<i64> {
            self.timeline
                .get(&pos)
                .and_then(|sn| self.events.get(sn))
                .map(|e| e.header().depth)
        };
        let top_depth = depth_at(top).ok_or_else(|| {
            RoomError::Internal("the event above a timeline gap is not held".into())
        })?;
        let below_depth = depth_at(gap.below).unwrap_or(i64::MIN);
        let anchor_sn = *self
            .timeline
            .get(&gap.filled_to)
            .ok_or_else(|| RoomError::Internal("a timeline gap's lowest filled event".into()))?;

        // Select.
        let in_timeline = self.timeline_sns();
        let mut seen: HashSet<OwnedEventId> = HashSet::new();
        let mut older_than_gap: HashSet<OwnedEventId> = HashSet::new();
        let mut batch: Vec<Event> = Vec::with_capacity(events.len());
        for event in events {
            if !seen.insert(event.event_id().to_owned()) {
                continue;
            }
            let room_id = event
                .json()
                .get("room_id")
                .and_then(CanonicalJsonValue::as_str);
            if room_id != Some(self.room_id.as_str()) {
                tracing::warn!(
                    room_id = %self.room_id,
                    event_id = %event.event_id(),
                    claimed_room = room_id.unwrap_or("<none>"),
                    "dropping a gap event that is for another room"
                );
                continue;
            }
            if self
                .event_id_index
                .get(event.event_id())
                .is_some_and(|sn| in_timeline.contains(sn))
            {
                continue;
            }
            let depth = event.header().depth;
            if depth > top_depth {
                tracing::warn!(
                    room_id = %self.room_id,
                    event_id = %event.event_id(),
                    depth,
                    top_depth,
                    "dropping a gap event newer than the event the gap sits below"
                );
                continue;
            }
            if depth <= below_depth {
                older_than_gap.insert(event.event_id().to_owned());
                continue;
            }
            batch.push(event);
        }
        if batch.is_empty() {
            self.close_gap(top)?;
            tracing::debug!(room_id = %self.room_id, top, "a timeline gap is closed: the answer held nothing new");
            return Ok(GapFill {
                added: 0,
                closed: true,
            });
        }
        batch.sort_by(|a, b| topological_order(b, a)); // newest first
        let room_left = usize::try_from(gap.filled_to - gap.below - 1).unwrap_or(0);
        let exhausted = batch.len() > room_left;
        if exhausted {
            tracing::warn!(
                room_id = %self.room_id,
                top,
                fetched = batch.len(),
                room_left,
                "a timeline gap has run out of positions; the oldest of its history is left on the other server"
            );
            batch.truncate(room_left);
        }

        // The walk, with the room as this server last had it as the fallback for keys the batch
        // does not set earlier.
        let fallback = match self.timeline.get(&gap.below) {
            Some(&below_sn) => self.state_map_after(below_sn)?,
            None => BTreeMap::new(),
        };
        let states_before = self.walk_history_states(anchor_sn, &batch, &fallback)?;
        let mut cited: BTreeSet<OwnedEventId> = gap.missing.clone();
        for event in &batch {
            cited.extend(pipeline::decode_event_ids(event.json().get("prev_events")));
        }
        let placed = batch.len();
        let planned: Vec<PlannedHistory> = batch
            .into_iter()
            .zip(states_before)
            .zip(1i64..)
            .map(|((event, state_before), i)| PlannedHistory {
                held_as: self.event_id_index.get(event.event_id()).copied(),
                event,
                room_pos: gap.filled_to - i,
                state_before,
            })
            .collect();
        let added = self.place_history(planned)?;

        let in_timeline = self.timeline_sns();
        let missing: BTreeSet<OwnedEventId> = cited
            .into_iter()
            .filter(|id| !older_than_gap.contains(id))
            .filter(|id| {
                !self
                    .event_id_index
                    .get(id)
                    .is_some_and(|sn| in_timeline.contains(sn))
            })
            .collect();
        let closed = exhausted || missing.is_empty();
        let filled_to = gap.filled_to - i64::try_from(placed).unwrap_or(i64::MAX);
        self.gaps.insert(
            top,
            Gap {
                below: gap.below,
                filled_to,
                missing,
                closed: false,
            },
        );
        if closed {
            self.close_gap(top)?;
        }
        tracing::debug!(
            room_id = %self.room_id,
            top,
            added,
            filled_to,
            closed,
            "placed history from a timeline gap"
        );
        Ok(GapFill { added, closed })
    }

    /// Marks the gap below `top` closed, in memory and durably.
    fn close_gap(&mut self, top: i64) -> Result<(), RoomError> {
        let Some(gap) = self.gaps.get_mut(&top) else {
            return Ok(());
        };
        gap.closed = true;
        gap.missing.clear();
        let bytes = serde_json::to_vec(&TimelineGapRecord {
            below: gap.below,
            closed: true,
        })
        .map_err(|e| RoomError::Internal(e.to_string()))?;
        let room_sn = self.room_sn;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.tables
                .timeline_gaps
                .put(txn, &(room_sn, top), &bytes)
                .map_err(to_kv)
        })
        .map_err(RoomError::from)
    }

    /// The room's state immediately after the timeline event `sn` (that event included), keyed
    /// by `(type, state_key)`, as event IDs.
    fn state_map_after(
        &self,
        sn: EventSn,
    ) -> Result<BTreeMap<(String, String), OwnedEventId>, RoomError> {
        let root = self
            .store
            .state_at(sn)
            .map_err(|e| RoomError::State(e.to_string()))?;
        let diff = self
            .store
            .diff(self.store.empty_root(), root)
            .map_err(|e| RoomError::State(e.to_string()))?;
        let mut after = BTreeMap::new();
        for s in diff.added.values() {
            if let Some(event) = self.events.get(s)
                && let Some(state_key) = event.header().state_key.clone()
            {
                after.insert(
                    (event.header().event_type.clone(), state_key),
                    event.event_id().to_owned(),
                );
            }
        }
        Ok(after)
    }
}
