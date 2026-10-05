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
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use ruma::OwnedEventId;

use super::history::GapSelection;
use super::{RoomActor, to_kv};
use crate::backfill::{GapAnchor, GapFill, HistoryKind};
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
    pub(super) below: i64,
    /// The lowest position held in `(below, top]`: the top itself until something is placed.
    pub(super) filled_to: i64,
    /// Event IDs the gap's events (its top included) cite as `prev_events` that are not in the
    /// timeline: where the next fetch walks back from.
    pub(super) missing: BTreeSet<OwnedEventId>,
    /// Nothing more will be fetched for it.
    pub(super) closed: bool,
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
    /// either), with the state at each event walked rather than fetched: exactly
    /// [`RoomActor::accept_history`] with [`HistoryKind::Gap`] and no fetched state, which is
    /// what `crate::backfill`'s implementation calls when it can ask for the state.
    ///
    /// # Where the events go
    /// Inside the gap below position `top`: the newest of the batch just below the lowest
    /// position filled so far, the oldest furthest down, ordered by `depth`, then
    /// `origin_server_ts`, then event ID. Skipped: events for another room, events already in
    /// the timeline, events with a `depth` above the top's (not history of it), and events no
    /// deeper than the event the gap sits above (`below`'s) -- history from before the leave,
    /// which is either held already or older than this server's first join and belongs below
    /// the timeline. Outliers -- the state the rejoin brought, a rename made while this server
    /// was out -- are placed, keep their outlier flag, and get the state computed for their
    /// position. When the batch is larger than the positions left, the newest that fit are
    /// placed and the gap is closed. An event that fails authorization is not placed
    /// (`super::history`).
    ///
    /// # The state at each event
    /// Walked back from the state before the lowest filled event (the rejoin's own explicit
    /// snapshot, for the first batch), exactly as [`RoomActor::accept_backfilled_events`] does,
    /// except that a key the batch holds no earlier setting for reverts to its value in the
    /// state after the `below` event (the room as this server last had it) rather than to
    /// nothing. Exact when the gap is filled in one batch and the room's history is linear; the
    /// exact way is the fetched state ([`RoomActor::accept_history`]).
    ///
    /// # Closing
    /// The gap is closed, durably, when every event it cites is in the timeline (or was
    /// rejected), when the batch held nothing new, or when its positions ran out.
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
        let outcome = self.accept_history(HistoryKind::Gap { top }, events, None)?;
        Ok(GapFill {
            added: outcome.added,
            closed: outcome.gap_closed,
        })
    }

    /// Records what placing a batch in a gap did to it: how far it is filled, what it still
    /// lacks (`cited`, the `prev_events` of what was placed, plus what it lacked before, less
    /// what is in the timeline now, what was rejected, and what is older than the gap), and
    /// whether it is closed. Returns whether it is.
    pub(super) fn finish_gap_fill(
        &mut self,
        selection: GapSelection,
        mut cited: BTreeSet<OwnedEventId>,
        placed: usize,
        exhausted: bool,
    ) -> Result<bool, RoomError> {
        let GapSelection {
            top,
            gap,
            older_than_gap,
        } = selection;
        cited.extend(gap.missing.iter().cloned());
        let in_timeline = self.timeline_sns();
        let missing: BTreeSet<OwnedEventId> = cited
            .into_iter()
            .filter(|id| !older_than_gap.contains(id) && !self.rejected_history.contains(id))
            .filter(|id| {
                !self
                    .event_id_index
                    .get(id)
                    .is_some_and(|sn| in_timeline.contains(sn))
            })
            .collect();
        // An event of another room is never part of this room's history, however its events
        // cite it (`RoomActor::held_in_another_room`): the gap does not wait for it.
        let mut missing = missing;
        if !missing.is_empty() {
            let cited: Vec<OwnedEventId> = missing.iter().cloned().collect();
            for id in cited {
                if self
                    .held_in_another_room(std::slice::from_ref(&id))?
                    .is_some()
                {
                    tracing::info!(
                        room_id = %self.room_id,
                        top,
                        event_id = %id,
                        "an event placed in a timeline gap cites an event of another room; the gap does not wait for it"
                    );
                    missing.remove(&id);
                }
            }
        }
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
            placed,
            filled_to,
            closed,
            "placed history from a timeline gap"
        );
        Ok(closed)
    }

    /// Marks the gap below `top` closed, in memory and durably.
    pub(super) fn close_gap(&mut self, top: i64) -> Result<(), RoomError> {
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
                .map_err(to_kv)?;
            super::catch_up::bump_rewrites(&self.tables, txn, room_sn)
        })
        .map_err(RoomError::from)
    }

    /// The room's state immediately after the timeline event `sn` (that event included), keyed
    /// by `(type, state_key)`, as event IDs.
    pub(super) fn state_map_after(
        &self,
        sn: EventSn,
    ) -> Result<BTreeMap<(String, String), OwnedEventId>, RoomError> {
        let root = self.root_after(sn)?;
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
